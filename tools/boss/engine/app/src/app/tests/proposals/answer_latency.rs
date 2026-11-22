//! A person's answer to a question card does not wait for the scheduler's
//! queued-display pass, or for the launch of work the answer created.
//!
//! The pass recomputes every queued card's label inside one read transaction
//! and holds its connection throughout. It runs on its own read-only
//! connection because that hold lasts for every queued card, and on the
//! shared connection that time would be added to every mutation that arrives
//! mid-pass. Launching the run an answer created (workspace lease, pane
//! spawn) runs off the session handler after the reply is enqueued.

use super::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

use boss_engine_dispatch::coordinator::DispatchPauseOrigin;
use boss_execution_contracts::{
    CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus, ExecutionRunner,
    RunOutcome,
};
use boss_protocol::{ExecutionStatus, PauseReason, ProposalKind};

/// How long a person's own action may take to be recorded and replied to.
const ANSWER_BOUND: Duration = Duration::from_millis(300);

/// Longest a pass stays held if nothing releases it, so an answer that waits
/// for the pass reports how late it was instead of hanging the suite.
const PASS_HOLD_LIMIT: Duration = Duration::from_secs(2);

/// Holds a queued-display pass inside its first statement, on its connection.
#[derive(Default)]
struct PassGate {
    /// (the pass reached the gate, the pass may continue)
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl PassGate {
    /// Called from the pass's connection as its statements run.
    fn hold(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        let deadline = Instant::now() + PASS_HOLD_LIMIT;
        while !state.1 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
        // A pass held once runs to completion.
        state.1 = true;
    }

    fn reached(&self) -> bool {
        self.state.lock().unwrap().0
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_is_not_queued_behind_a_slow_queued_display_pass() {
    let (state, _dir, asking, task_id) = parked_question_task().await;
    let coordinator = state.execution_coordinator.clone();

    // A slow pass: every statement it runs on its connection stops at the gate.
    let gate = Arc::new(PassGate::default());
    let display_db = coordinator.queued_display_db();
    display_db.connect().unwrap().progress_handler(
        1,
        Some({
            let gate = gate.clone();
            move || {
                gate.hold();
                false
            }
        }),
    );
    let pass = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            let mut displays = None;
            coordinator
                .refresh_queued_displays(&mut displays, boss_engine_utils::epoch_time::now_epoch_secs())
                .await;
            displays
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !gate.reached() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the queued-display pass must read through the connection it is given");

    // The click, while the pass is still holding its connection.
    let clicked = Instant::now();
    let reply = answer_question(&state, &task_id, true).await;
    let answered = clicked.elapsed();
    let pass_still_reading = !pass.is_finished();
    eprintln!(
        "answer latency during a held queued-display pass: {answered:?} (pass still reading: {pass_still_reading})"
    );

    let FrontendEvent::WorkItemUpdated { item } = reply else {
        panic!("expected the answered card, got {reply:?}");
    };
    let (boss_protocol::WorkItem::Task(card) | boss_protocol::WorkItem::Chore(card)) = item else {
        panic!("expected the answered task");
    };
    assert_eq!(card.id, task_id);
    assert!(card.operator_question.is_none(), "the reply clears the question card");
    assert!(
        answered < ANSWER_BOUND,
        "an answer must be recorded and replied to within {ANSWER_BOUND:?} while a queued-display pass is \
         reading; it took {answered:?}"
    );
    assert!(
        pass_still_reading,
        "the answer completed while the queued-display pass was still holding its connection"
    );
    assert_eq!(
        resume_after_answer(&state, &task_id, &asking),
        1,
        "the answer is durable: it minted the run that carries it"
    );

    gate.release();
    let displays = pass.await.unwrap();
    display_db.connect().unwrap().progress_handler(0, None::<fn() -> bool>);
    assert!(
        displays.is_some(),
        "the held pass still completes and seeds its snapshot"
    );
}

/// First workspace lease returns at once; later leases wait until [`Self::release`].
struct HeldLeaseCube {
    leases: AtomicUsize,
    hold_started: AtomicBool,
    hold_finished: AtomicBool,
    released: AtomicBool,
    release: Notify,
}

impl HeldLeaseCube {
    fn new() -> Self {
        Self {
            leases: AtomicUsize::new(0),
            hold_started: AtomicBool::new(false),
            hold_finished: AtomicBool::new(false),
            released: AtomicBool::new(false),
            release: Notify::new(),
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

boss_execution_contracts::stub_cube_client! { HeldLeaseCube {
    async fn ensure_repo(&self, _origin: &str) -> anyhow::Result<CubeRepoHandle> {
        Ok(CubeRepoHandle {
            repo_id: "test-repo".to_owned(),
        })
    }
    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        prefer_workspace_id: Option<&str>,
        _allow_dirty: bool,
        _exclude_workspace_ids: &[&str],
    ) -> anyhow::Result<CubeWorkspaceLease> {
        if self.leases.fetch_add(1, Ordering::SeqCst) > 0 {
            self.hold_started.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + PASS_HOLD_LIMIT;
            while !self.released.load(Ordering::SeqCst) {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = tokio::time::sleep(left) => { break; }
                }
            }
            self.hold_finished.store(true, Ordering::SeqCst);
        }
        let workspace_id = prefer_workspace_id
            .map(str::to_owned)
            .unwrap_or_else(|| "test-workspace-001".to_owned());
        Ok(CubeWorkspaceLease {
            lease_id: "test-lease-1".to_owned(),
            workspace_path: std::path::PathBuf::from(format!("/tmp/{workspace_id}")),
            workspace_id,
            dirty_verified: None,
        })
    }
    async fn create_change(
        &self,
        _workspace_path: &Path,
        _title: &str,
    ) -> anyhow::Result<CubeChangeHandle> {
        Ok(CubeChangeHandle {
            change_id: "test-change-1".to_owned(),
        })
    }
    async fn goto_workspace(&self, _workspace_path: &Path, _pr: u64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn goto_workspace_revision(&self, _workspace_path: &Path, _sha: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn release_workspace(&self, _lease_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn workspace_status(
        &self,
        workspace_path: &Path,
    ) -> anyhow::Result<CubeWorkspaceStatus> {
        Ok(CubeWorkspaceStatus::builder()
            .workspace_id("test-workspace-001")
            .workspace_path(workspace_path.to_path_buf())
            .state("leased")
            .lease_id("test-lease-1")
            .holder("boss/0")
            .task("test task")
            .leased_at_epoch_s(1_700_000_000)
            .lease_expires_at_epoch_s(1_700_001_800)
            .build())
    }
    async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: Option<u64>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _lease_id: &str, _reason: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> anyhow::Result<Vec<CubeWorkspaceStatus>> {
        Ok(vec![])
    }
    async fn list_repos(&self) -> anyhow::Result<Vec<CubeRepoSummary>> {
        Ok(vec![])
    }
}}

#[derive(Default)]
struct HeldLaunchRunner(std::sync::atomic::AtomicUsize, tokio::sync::Notify);

#[async_trait::async_trait]
impl ExecutionRunner for HeldLaunchRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        _execution: &boss_protocol::WorkExecution,
        _work_item: &boss_protocol::WorkItem,
        _workspace_path: &std::path::Path,
        _cube_change_id: Option<&str>,
    ) -> anyhow::Result<RunOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.notify_one();
        std::future::pending().await
    }
}

async fn force_launched_question_with_held_lease() -> (
    Arc<ServerState>,
    tempfile::TempDir,
    String,
    String,
    Arc<HeldLeaseCube>,
    Arc<HeldLaunchRunner>,
) {
    let cube = Arc::new(HeldLeaseCube::new());
    let runner = Arc::new(HeldLaunchRunner::default());
    let (state, dir) = test_server_state_with_overrides(ServerStateOverrides {
        cube_client: Some(cube.clone()),
        execution_runner: Some(runner.clone()),
        ..Default::default()
    });
    state.feature_flags.set("worker_proposals", true).unwrap();
    state.feature_flags.set("run_done_proposals_seam", true).unwrap();
    state.execution_coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: investigating worker failures").unwrap(),
    );
    let product = boss_engine_runtime::test_support::create_test_product(&state.work_db);
    let task =
        boss_engine_runtime::test_support::create_test_chore(&state.work_db, product.id, "Force launched question");
    let task_id = task.id;
    let asking = state
        .work_db
        .request_execution(
            boss_protocol::RequestExecutionInput::builder()
                .work_item_id(&task_id)
                .build(),
        )
        .unwrap()
        .id;
    state
        .work_db
        .update_work_item(
            &task_id,
            boss_protocol::WorkItemPatch::builder()
                .description("Original brief")
                .build(),
        )
        .unwrap();
    state
        .execution_coordinator
        .force_dispatch(&asking, boss_execution_contracts::DispatchAdmission::OperatorForced)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner.0.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the asking run must start so force-launch lineage is recorded");
    assert_eq!(
        state.work_db.get_execution(&asking).unwrap().status,
        ExecutionStatus::Running
    );
    state
        .worker_registry
        .register(std::process::id() as libc::pid_t, asking.clone());
    submitted(
        call_with_peer(
            &state,
            Some(std::process::id() as libc::pid_t),
            submit_request(&asking, ProposalKind::RunDone, operator_question_payload()),
        )
        .await,
    );
    boss_engine_runtime::test_support::release_finished_execution_lease(&state.work_db, &asking);
    (state, dir, asking, task_id, cube, runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_that_launches_work_is_not_queued_behind_a_slow_cube_lease() {
    let (state, _dir, asking, task_id, cube, _runner) = force_launched_question_with_held_lease().await;

    let clicked = Instant::now();
    let reply = answer_question(&state, &task_id, true).await;
    let answered = clicked.elapsed();

    tokio::time::timeout(Duration::from_secs(5), async {
        while !cube.hold_started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the launched resume must reach cube lease after the reply");

    let launch_still_pending = !cube.hold_finished.load(Ordering::SeqCst);
    eprintln!("answer latency during a held cube lease: {answered:?} (launch still pending: {launch_still_pending})");

    let FrontendEvent::WorkItemUpdated { item } = reply else {
        panic!("expected the answered card, got {reply:?}");
    };
    let (boss_protocol::WorkItem::Task(card) | boss_protocol::WorkItem::Chore(card)) = item else {
        panic!("expected the answered task");
    };
    assert_eq!(card.id, task_id);
    assert!(card.operator_question.is_none(), "the reply clears the question card");
    assert!(
        answered < ANSWER_BOUND,
        "an answer must be recorded and replied to within {ANSWER_BOUND:?} while the launch is \
         still waiting for a cube lease; it took {answered:?}"
    );
    assert!(
        launch_still_pending,
        "the answer completed while launch_answered_work was still waiting for the cube lease"
    );
    assert_eq!(
        resume_after_answer(&state, &task_id, &asking),
        1,
        "the answer is durable: it minted the run that carries it"
    );

    cube.release();
}

fn resume_after_answer(state: &ServerState, task_id: &str, asking: &str) -> usize {
    state
        .work_db
        .list_executions(Some(task_id))
        .unwrap()
        .into_iter()
        .filter(|execution| execution.id != asking)
        .count()
}
