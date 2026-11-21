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
    NoopWorkerNudger, RecoveryContext, TransientRecoveryOutcome, TransientRecoveryReaper, run_one_pass_with_occupancy,
};
use crate::workspace_lease_release::{
    DbPreservation, EmptyOccupancy, HostOccupancy, KeepReason, LEASE_OCCUPANCY_KEPT_ATTENTION_KIND,
    LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND, LEASE_UNRELEASED_ATTENTION_KIND, LeaseReleaseOutcome, Occupancy,
    PaneStatus, ReleaseEvidence, TerminalCause, WorkspaceOccupancy, apply_release,
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

    async fn drain(&self) {
        self.coordinator.drain_ready_queue_for_test().await;
        self.coordinator.wait_predecessor_rechecks_for_test().await;
        self.coordinator.drain_ready_queue_for_test().await;
        self.coordinator.wait_predecessor_rechecks_for_test().await;
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
            host_adapters: &NoHosts,
        };
        // These fixtures own empty workspaces; host processes cannot supply their evidence.
        let outcome = run_one_pass_with_occupancy(
            &cx,
            &mut HashSet::new(),
            boss_engine_utils::epoch_time::now_epoch_secs(),
            Some(&EmptyOccupancy),
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
    s.drain().await;
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
async fn orphan_cleanup_preserves_work_and_unblocks_the_successor() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    let mut child = spawn_setsid_sleeper(&s.repo.worker, |command| {
        command.env("BOSS_RUN_ID", &s.stalled_id);
    });
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_some());
    let started = std::time::Instant::now();
    let result = apply_release(
        s.db(),
        &s.stalled(),
        "lease-old",
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &crate::workspace_lease_release::HostOccupancy,
            preservation: &DbPreservation::new(s.db(), None).with_cube(s.cube.as_ref()),
        },
        s.cube.force_release_lease("lease-old", Some("orphan cleanup")),
    )
    .await;
    println!(
        "orphan cleanup and preservation elapsed_ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(
        result,
        LeaseReleaseOutcome::Released,
        "{:?}",
        s.db().list_attention_items(&s.stalled_id).unwrap()
    );
    assert!(child.try_wait().unwrap().is_some());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert!(s.db().recovery_predecessor_holding_lease(&successor).unwrap().is_none());
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

fn spawn_workspace_occupant(workspace: &Path, run_id: Option<&str>) -> crate::test_support::ProcessGroupChild {
    spawn_setsid_sleeper(workspace, |command| {
        command.env_remove("BOSS_RUN_ID").env_remove("BOSS_SPAWN_TOKEN");
        if let Some(run_id) = run_id {
            command.env("BOSS_RUN_ID", run_id);
        }
    })
}

fn wait_until_occupied(path: &Path) -> Occupancy {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let seen = HostOccupancy.occupancy(path);
        if seen == Occupancy::Occupied || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn wait_until_empty(path: &Path) -> Occupancy {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let seen = HostOccupancy.occupancy(path);
        if seen == Occupancy::Empty || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

async fn keep_lease_with_occupants(s: &Stalled) -> LeaseReleaseOutcome {
    apply_release(
        s.db(),
        &s.stalled(),
        "lease-old",
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &HostOccupancy,
            preservation: &DbPreservation::new(s.db(), None).with_cube(s.cube.as_ref()),
        },
        s.cube.force_release_lease("lease-old", Some("occupancy keep")),
    )
    .await
}

#[tokio::test]
async fn occupancy_kept_lease_releases_on_recheck_and_successor_dispatches() {
    release_after_occupants_exit(true).await;
}

#[tokio::test]
async fn dispatch_recheck_releases_empty_workspace_without_a_sweep() {
    release_after_occupants_exit(false).await;
}

async fn release_after_occupants_exit(run_sweep_first: bool) {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    let occupant = spawn_workspace_occupant(&s.repo.worker, None);
    assert_eq!(wait_until_occupied(&s.repo.worker), Occupancy::Occupied);
    let kept = keep_lease_with_occupants(&s).await;
    assert!(
        matches!(
            kept,
            LeaseReleaseOutcome::Kept {
                reason: KeepReason::OccupancyUnconfirmed | KeepReason::ProcessInWorkspace
            }
        ),
        "{kept:?}"
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.wait_reason(&successor.id).as_deref(), Some(HOLD_REASON));

    drop(occupant);
    assert_eq!(wait_until_empty(&s.repo.worker), Occupancy::Empty);
    if run_sweep_first {
        let swept =
            crate::workspace_lease_release::run_one_pass(s.db(), s.cube.as_ref(), &LiveWorkerStateRegistry::new())
                .await;
        assert_eq!(swept.released, 1, "{swept:?}");
    } else {
        age_predecessor(&s);
        s.drain().await;
    }
    assert!(s.db().get_execution(&s.stalled_id).unwrap().cube_lease_id.is_none());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());

    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

#[tokio::test]
async fn successor_failure_quotes_fresh_occupancy_not_the_original_list() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    let original = spawn_workspace_occupant(&s.repo.worker, None);
    assert_eq!(wait_until_occupied(&s.repo.worker), Occupancy::Occupied);
    assert!(matches!(
        keep_lease_with_occupants(&s).await,
        LeaseReleaseOutcome::Kept { .. }
    ));
    let original_pid = original.id().to_string();
    let original_body = s
        .db()
        .list_attention_items(&s.stalled_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND)
        .unwrap()
        .body_markdown;
    assert!(original_body.contains(&original_pid), "{original_body}");

    drop(original);
    assert_eq!(wait_until_empty(&s.repo.worker), Occupancy::Empty);

    let product = create_test_product(s.db());
    let chore = create_test_chore(s.db(), product.id, "live-owner");
    let live = create_ready_chore_execution(s.db(), chore.id);
    s.db()
        .start_execution_run(&live.id, "worker", "mono", "lease-live", "ws-live", "/tmp/ws-live")
        .unwrap();
    let live_occupant = spawn_workspace_occupant(&s.repo.worker, Some(&live.id));
    assert_eq!(wait_until_occupied(&s.repo.worker), Occupancy::Occupied);
    let live_pid = live_occupant.id().to_string();

    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    age_predecessor(&s);
    let err = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("Recovery cannot start"), "{err}");
    assert!(
        err.contains(&live_pid),
        "fresh scan must name the current occupant: {err}"
    );
    assert!(
        !err.contains(&original_pid),
        "stale occupants must not appear in the successor failure: {err}"
    );
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Failed
    );
    drop(live_occupant);
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
    s.drain().await;
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
    s.drain().await;
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
    s.drain().await;
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
async fn inline_reconcile_fails_loudly_when_preservation_cannot_proceed_then_allows_retry() {
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
    // The kept lease raised an attention item, but one fresh item is not
    // proof the problem is permanent: the sweeps retry, so the row only waits.
    assert!(
        s.db()
            .list_attention_items(&s.stalled_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == boss_engine_storage_values::attention_kinds::LEASE_UNRELEASED_ATTENTION_KIND)
    );
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(s.wait_reason(&next.id).as_deref(), Some(HOLD_REASON));
    assert!(s.cube.lease_calls.lock().await.is_empty());

    // Once the predecessor has stayed unreleased past the bound the successor
    // fails loudly, naming the predecessor's recorded cleanup problem.
    age_predecessor(&s);
    let err = s
        .coordinator
        .force_dispatch(&next.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("Recovery cannot start"), "{err:#}");
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(
        s.wait_reason(&next.id),
        None,
        "a successor that waited and then failed through force-dispatch keeps no wait reason"
    );
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
    // A successor created after the problem was already old has not yet had
    // the sweeps' full bound under its own clock: it waits before it can fail.
    s.drain().await;
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(s.wait_reason(&next.id).as_deref(), Some(HOLD_REASON));
    age_waits(&s);
    s.drain().await;
    assert_eq!(s.db().get_execution(&next.id).unwrap().status, ExecutionStatus::Failed);
    assert_eq!(
        s.wait_reason(&next.id),
        None,
        "a row that waited and then failed through the scheduler keeps no wait reason"
    );

    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET workspace_path = ?2 WHERE id = ?1",
            rusqlite::params![s.stalled_id, s.repo.worker.to_str().unwrap()],
        )
        .unwrap();
    assert_eq!(s.release_like_a_later_sweep().await, LeaseReleaseOutcome::Released);
    let next = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    wait_for_execution_status(s.db(), &next.id, ExecutionStatus::Running).await;
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

/// Make the stalled predecessor look like it ended an hour ago, and any
/// kept-lease attention item look like it was filed then.
fn age_predecessor(s: &Stalled) {
    let hour_ago = (boss_engine_utils::epoch_time::now_epoch_secs() - 3600).to_string();
    {
        let conn = s.db().connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
            rusqlite::params![s.stalled_id, hour_ago],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_attention_items SET created_at = ?2 WHERE execution_id = ?1",
            rusqlite::params![s.stalled_id, hour_ago],
        )
        .unwrap();
    }
    age_waits(s);
}

/// Make every successor's hold look like it began an hour ago.
fn age_waits(s: &Stalled) {
    let hour_ago = (boss_engine_utils::epoch_time::now_epoch_secs() - 3600).to_string();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_since = ?1 WHERE dispatch_wait_reason IS NOT NULL",
            [&hour_ago],
        )
        .unwrap();
}

#[tokio::test]
async fn terminal_remote_predecessor_with_unreachable_host_fails_the_successor_after_the_bound() {
    // Missing cleanup evidence begins a bounded wait. A failed owning-host
    // lookup then supplies durable diagnostics without extending its clock.
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    age_predecessor(&s);
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready,
        "an unswept local predecessor is waited for"
    );
    assert_eq!(s.wait_reason(&successor.id).as_deref(), Some(HOLD_REASON));

    s.db().add_host("remote", "user@remote", 4, &[]).unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET host_id = 'remote' WHERE execution_id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    // The remote evidence is old, but this successor's own hold only just began.
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );

    // Reaching the bound forces a fresh host lookup and records its error.
    age_waits(&s);
    s.drain().await;
    // A freshly filed release failure is still being retried: it keeps the
    // successor waiting however old the remote evidence and its hold are.
    crate::workspace_lease_release::raise_attention(
        s.db(),
        &s.stalled(),
        boss_engine_storage_values::attention_kinds::LEASE_RELEASE_FAILED_ATTENTION_KIND,
        "Workspace lease release failed",
        "remote unavailable",
    );
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready,
        "a freshly filed release failure keeps the successor waiting"
    );
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_attention_items SET status = 'resolved' WHERE execution_id = ?1 AND kind = 'workspace_lease_release_failed'",
            [&s.stalled_id],
        )
        .unwrap();

    age_predecessor(&s);
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(s.wait_reason(&successor.id), None);
}

#[tokio::test]
async fn local_predecessor_cube_release_failure_fails_the_successor_after_the_bound() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();

    assert!(
        matches!(fail_cube_release(&s).await, LeaseReleaseOutcome::CubeFailed { .. }),
        "{:?}",
        s.db().list_attention_items(&s.stalled_id).unwrap()
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert!(s.db().list_attention_items(&s.stalled_id).unwrap().iter().any(|item| {
        item.kind == boss_engine_storage_values::attention_kinds::LEASE_RELEASE_FAILED_ATTENTION_KIND
            && item.status == "open"
    }));

    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.wait_reason(&successor.id).as_deref(), Some(HOLD_REASON));

    // The leftover-lease sweep keeps retrying cube while the successor waits.
    assert!(matches!(
        fail_cube_release(&s).await,
        LeaseReleaseOutcome::CubeFailed { .. }
    ));
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready,
        "a fresh cube-release failure is retried, not a successor failure"
    );

    age_predecessor(&s);
    let err = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    let err = format!("{err:#}");
    assert!(err.contains("Recovery cannot start"), "{err}");
    assert!(err.contains("cube workspace release failed"), "{err}");
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(s.wait_reason(&successor.id), None);
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(
        matches!(fail_cube_release(&s).await, LeaseReleaseOutcome::CubeFailed { .. }),
        "sweeps keep retrying cube after the successor has failed"
    );
}

async fn fail_cube_release(s: &Stalled) -> LeaseReleaseOutcome {
    let stalled = s.stalled();
    let preservation = DbPreservation::new(s.db(), None).with_cube(s.cube.as_ref());
    apply_release(
        s.db(),
        &stalled,
        "lease-old",
        ReleaseEvidence {
            cause: TerminalCause::Inferred,
            pane: PaneStatus::Gone,
            occupancy: &EmptyOccupancy,
            preservation: &preservation,
        },
        async { Err(anyhow::anyhow!("cube workspace release failed")) },
    )
    .await
}

/// Every run in these tests is local, so no host adapter is ever requested.
struct NoHosts;

#[async_trait::async_trait]
impl boss_execution_contracts::HostAdapterProvider for NoHosts {
    async fn adapter_for(
        &self,
        host: &crate::host_registry::Host,
    ) -> anyhow::Result<std::sync::Arc<dyn boss_execution_contracts::HostAdapter>> {
        anyhow::bail!("unexpected adapter request for host {}", host.id)
    }
}

#[tokio::test]
async fn successor_never_signals_a_recorded_live_shell() {
    for occupancy_evidence in [false, true] {
        let s = Stalled::new(true).await;
        let mut shell = spawn_workspace_occupant(&s.repo.worker, Some(&s.stalled_id));
        s.db()
            .set_run_shell_pid_for_execution(&s.stalled_id, i64::from(shell.id()))
            .unwrap();
        s.db()
            .mark_execution_orphaned(&s.stalled_id, "abort unconfirmed")
            .unwrap();
        crate::workspace_lease_release::raise_attention(
            s.db(),
            &s.stalled(),
            LEASE_UNRELEASED_ATTENTION_KIND,
            "Terminal worker still holds its workspace",
            "Shell death unconfirmed",
        );
        if occupancy_evidence {
            crate::workspace_lease_release::raise_attention(
                s.db(),
                &s.stalled(),
                crate::workspace_lease_release::LEASE_OCCUPANCY_KEPT_ATTENTION_KIND,
                "Prior occupancy hold",
                "Pane previously gone",
            );
        }
        let successor = s
            .db()
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(s.stalled().work_item_id)
                    .build(),
            )
            .unwrap();
        s.drain().await;
        assert_eq!(
            s.db().get_execution(&successor.id).unwrap().status,
            ExecutionStatus::Ready
        );
        assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
        assert!(shell.try_wait().unwrap().is_none(), "the shell must not be signalled");
        assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());
    }
}

#[tokio::test]
async fn no_pid_unconfirmed_death_has_a_bounded_successor_wait() {
    let s = Stalled::new(true).await;
    s.db()
        .mark_execution_orphaned(&s.stalled_id, "spawn cancelled")
        .unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    let outcome =
        crate::workspace_lease_release::run_one_pass(s.db(), s.cube.as_ref(), &LiveWorkerStateRegistry::new()).await;
    assert_eq!(outcome.held, 1);
    assert!(
        s.db()
            .list_attention_items(&s.stalled_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND)
    );
    s.drain().await;
    age_predecessor(&s);
    let err = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("death is unconfirmed"), "{err:#}");
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
}

struct ChangingRemoteOccupants {
    listing_fails: std::sync::atomic::AtomicBool,
    probe_fault: std::sync::atomic::AtomicUsize,
    error: std::sync::Mutex<String>,
    calls: std::sync::atomic::AtomicUsize,
}

crate::stub_host_adapter! { ChangingRemoteOccupants {
    fn host_id(&self) -> &str { "remote" }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> {
        anyhow::ensure!(!self.listing_fails.load(Ordering::SeqCst), "registry unavailable");
        Ok(vec![CubeWorkspaceStatus::builder().workspace_id("workspace-old").workspace_path("/remote/workspace".into()).state("leased").lease_id("lease-old").build()])
    }
    async fn probe_remote_worker_alive(&self, _: i64) -> anyhow::Result<Option<bool>> {
        match self.probe_fault.load(Ordering::SeqCst) {
            1 => Ok(None),
            2 => anyhow::bail!("probe unavailable"),
            _ => Ok(Some(false)),
        }
    }
    async fn cleanup_execution_processes(&self, _: &str, _: &Path) -> anyhow::Result<Occupancy> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("{}", self.error.lock().unwrap())
    }
} }

struct ChangingRemoteProvider(Arc<ChangingRemoteOccupants>);
#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for ChangingRemoteProvider {
    async fn adapter_for(
        &self,
        _: &crate::host_registry::Host,
    ) -> anyhow::Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn remote_successor_failure_quotes_only_the_latest_occupants() {
    let mut s = Stalled::new(true).await;
    let adapter = Arc::new(ChangingRemoteOccupants {
        listing_fails: Default::default(),
        probe_fault: Default::default(),
        error: std::sync::Mutex::new("original-occupant".to_owned()),
        calls: Default::default(),
    });
    Arc::get_mut(&mut s.coordinator)
        .unwrap()
        .set_host_adapter_provider(Arc::new(ChangingRemoteProvider(adapter.clone())));
    s.db().add_host("remote", "user@remote", 4, &[]).unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET host_id = 'remote', remote_pid = 4242 WHERE execution_id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET host_id = 'remote' WHERE id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    boss_engine_completion::workspace_lease_release::release_remote_lease(
        s.db(),
        adapter.as_ref(),
        &s.stalled_id,
        PaneStatus::Gone,
        |_| async { panic!("occupied lease must stay held") },
    )
    .await;
    s.drain().await;
    let calls = adapter.calls.load(std::sync::atomic::Ordering::SeqCst);
    s.drain().await;
    assert_eq!(
        adapter.calls.load(std::sync::atomic::Ordering::SeqCst),
        calls,
        "ordinary drains must throttle remote cleanup"
    );
    *adapter.error.lock().unwrap() = "current-occupant".to_owned();
    age_predecessor(&s);
    let created_at = s
        .db()
        .list_attention_items(&s.stalled_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND)
        .unwrap()
        .created_at;
    let err = s
        .coordinator
        .force_dispatch(&successor.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("current-occupant"), "{text}");
    assert!(!text.contains("original-occupant"), "{text}");
    let item = s
        .db()
        .list_attention_items(&s.stalled_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == LEASE_UNRELEASED_ATTENTION_KIND)
        .unwrap();
    assert_eq!(item.created_at, created_at, "refresh must preserve the hold clock");
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Failed
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
}

struct WrongHostProvider(Arc<dyn boss_execution_contracts::HostAdapter>);
#[async_trait]
impl boss_execution_contracts::HostAdapterProvider for WrongHostProvider {
    async fn adapter_for(
        &self,
        _: &crate::host_registry::Host,
    ) -> anyhow::Result<Arc<dyn boss_execution_contracts::HostAdapter>> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn remote_retry_rejects_a_provider_that_routes_to_local() {
    let mut s = Stalled::new(true).await;
    let local = s.coordinator.host_adapter();
    Arc::get_mut(&mut s.coordinator)
        .unwrap()
        .set_host_adapter_provider(Arc::new(WrongHostProvider(local)));
    s.db().add_host("remote", "user@remote", 4, &[]).unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET host_id = 'remote', remote_pid = 4242 WHERE execution_id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    s.db()
        .connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET host_id = 'remote' WHERE id = ?1",
            [&s.stalled_id],
        )
        .unwrap();
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    crate::workspace_lease_release::raise_attention(
        s.db(),
        &s.stalled(),
        crate::workspace_lease_release::LEASE_OCCUPANCY_KEPT_ATTENTION_KIND,
        "Prior occupancy hold",
        "Pane previously gone",
    );
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    let item = s
        .db()
        .list_attention_items(&s.stalled_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND)
        .unwrap();
    assert!(item.body_markdown.contains("reports host 'local'"), "{item:?}");
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
}

#[tokio::test]
async fn externally_released_lease_preserves_the_free_workspace() {
    reconcile_missing_lease(false).await;
}

#[tokio::test]
async fn reassigned_lease_preserves_only_the_predecessor_boundary() {
    reconcile_missing_lease(true).await;
}

fn legacy_occupancy_evidence(s: &Stalled) {
    crate::workspace_lease_release::raise_attention(
        s.db(),
        &s.stalled(),
        LEASE_UNRELEASED_ATTENTION_KIND,
        boss_engine_storage_values::attention_kinds::LEASE_PROCESS_CLEANUP_TITLE,
        "Workspace processes have not exited",
    );
}

#[tokio::test]
async fn absent_lease_inspection_still_lets_the_successor_start() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    legacy_occupancy_evidence(&s);
    JjRepo::snapshot(&s.repo.worker);
    let boundary = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    s.db()
        .confirm_workspace_lease(&s.stalled_id, "lease-old", boundary)
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    JjRepo::run(&s.repo.worker, &["new", "root()", "-m", "Intervening holder"]);
    std::fs::write(s.repo.worker.join("theirs"), "intervening").unwrap();
    s.cube.active_leases.lock().await.clear();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert!(s.stalled().cube_lease_id.is_none());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert!(
        s.db()
            .list_attention_items(&s.stalled_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == "preserved_work_needs_inspection" && item.status == "open")
    );
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

#[tokio::test]
async fn legacy_occupancy_keep_survives_a_transient_registry_fault() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    legacy_occupancy_evidence(&s);
    s.cube.list_workspaces_fail.store(true, Ordering::SeqCst);
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    let items = s.db().list_attention_items(&s.stalled_id).unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.kind == LEASE_OCCUPANCY_KEPT_ATTENTION_KIND && item.status == "open"),
        "{items:?}"
    );
    assert!(
        items.iter().any(|item| {
            item.kind == LEASE_RECOVERY_UNAVAILABLE_ATTENTION_KIND
                && item.title == "Workspace lease registry is unavailable"
        }),
        "{items:?}"
    );
    assert!(
        items.iter().any(|item| {
            item.kind == LEASE_UNRELEASED_ATTENTION_KIND
                && item.title == boss_engine_storage_values::attention_kinds::LEASE_PROCESS_CLEANUP_TITLE
        }),
        "{items:?}"
    );
    s.cube.list_workspaces_fail.store(false, Ordering::SeqCst);
    let swept =
        crate::workspace_lease_release::run_one_pass(s.db(), s.cube.as_ref(), &LiveWorkerStateRegistry::new()).await;
    assert_eq!(swept.released, 1, "{swept:?}");
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert!(s.stalled().cube_lease_id.is_none());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

#[tokio::test]
async fn legacy_occupancy_keep_dispatches_after_preservation() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    legacy_occupancy_evidence(&s);
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    assert!(
        s.db()
            .latest_local_shell_pid_for_execution(&s.stalled_id)
            .unwrap()
            .is_none()
    );
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert!(s.stalled().cube_lease_id.is_none());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
}

async fn reconcile_missing_lease(reassigned: bool) {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    legacy_occupancy_evidence(&s);
    s.db()
        .record_workspace_lease_generation(&s.stalled_id, "lease-old", 1)
        .unwrap();
    // Snapshot the predecessor while it still owns the lease.
    JjRepo::snapshot(&s.repo.worker);
    let boundary = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    s.db()
        .confirm_workspace_lease(&s.stalled_id, "lease-old", boundary)
        .unwrap();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.cube.active_leases.lock().await.clear();
    if reassigned {
        // New edits belong to another holder and must survive reconciliation.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        JjRepo::run(&s.repo.worker, &["new", "main", "-m", "new holder workspace"]);
        std::fs::write(s.repo.worker.join("fix"), "new holder work").unwrap();
        JjRepo::snapshot(&s.repo.worker);
    }
    s.cube.list_workspaces_response.lock().await.push(
        CubeWorkspaceStatus::builder()
            .workspace_id("workspace-old")
            .workspace_path(s.repo.worker.clone())
            .state(if reassigned { "leased" } else { "free" })
            .maybe_lease_id(reassigned.then(|| "lease-new".to_owned()))
            .lease_generation(if reassigned { 2 } else { 1 })
            .build(),
    );
    age_predecessor(&s);
    s.drain().await;
    wait_for_execution_status(s.db(), &successor.id, ExecutionStatus::Running).await;
    assert!(s.stalled().cube_lease_id.is_none());
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_some());
    assert_eq!(
        std::fs::read_to_string(s.repo.replacement.join("fix")).unwrap(),
        "unpublished fix"
    );
    assert_eq!(
        std::fs::read_to_string(s.repo.worker.join("fix")).unwrap(),
        if reassigned {
            "new holder work"
        } else {
            "unpublished fix"
        }
    );
    assert!(s.cube.force_release_calls.lock().await.is_empty());
    if reassigned {
        let rows = s.cube.list_workspaces_response.lock().await;
        assert_eq!(rows[0].lease_id.as_deref(), Some("lease-new"));
        assert_eq!(rows[0].state, "leased");
    }
}

#[tokio::test]
async fn absent_lease_preservation_failure_keeps_successor_held() {
    let s = Stalled::new(false).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    legacy_occupancy_evidence(&s);
    s.cube.active_leases.lock().await.clear();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    let swept =
        crate::workspace_lease_release::run_one_pass(s.db(), s.cube.as_ref(), &LiveWorkerStateRegistry::new()).await;
    assert_eq!(swept.kept, 1);
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    let items = s.db().list_attention_items(&s.stalled_id).unwrap();
    assert!(items.iter().any(|item| item.kind == "preserved_work_needs_inspection"
        && item.body_markdown.contains(s.repo.worker.to_str().unwrap())));
    age_predecessor(&s);
    s.drain().await;
    let failed = s.db().get_execution(&successor.id).unwrap();
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert!(s.db().list_runs(&successor.id).unwrap().iter().any(|run| {
        run.error_text
            .as_deref()
            .is_some_and(|error| error.contains(s.repo.worker.to_str().unwrap()))
    }));
}

#[tokio::test]
async fn absent_lease_does_not_authorize_unconfirmed_worker_death() {
    let s = Stalled::new(true).await;
    s.db().mark_execution_orphaned(&s.stalled_id, "worker gone").unwrap();
    s.cube.active_leases.lock().await.clear();
    let successor = s
        .db()
        .request_execution(
            RequestExecutionInput::builder()
                .work_item_id(s.stalled().work_item_id)
                .build(),
        )
        .unwrap();
    s.drain().await;
    assert_eq!(
        s.db().get_execution(&successor.id).unwrap().status,
        ExecutionStatus::Ready
    );
    assert_eq!(s.stalled().cube_lease_id.as_deref(), Some("lease-old"));
    assert!(s.db().preserved_tip(&s.stalled_id).unwrap().is_none());
}

#[path = "predecessor_recheck.rs"]
mod recheck;

#[path = "abandoned_predecessor.rs"]
mod abandoned;
