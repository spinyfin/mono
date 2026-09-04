//! Real-tmux integration coverage for the local worker recovery path.
//!
//! The fixture enters through `start_worker`, so it creates an actual private
//! tmux server and uses the same durable spawn ordering as a local worker.
//! Its substituted login shell continuously redraws and changes the terminal
//! title without producing a driver event. The test then simulates an engine
//! restart through tmux adoption and drives the production stale-reap and
//! orphan-redispatch passes with short, injected time windows.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use boss_engine::app::SendToAppError;
use boss_engine::coordinator::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    ExecutionCoordinator, WorkerPool,
};
use boss_engine::dispatch_events::RecordingDispatchEventSink;
use boss_engine::driver::ClaudeDriver;
use boss_engine::hold_registry::HoldRegistry;
use boss_engine::live_worker_state::LiveWorkerStateRegistry;
use boss_engine::orphan_sweep::run_one_pass_for_item_with_min_age;
use boss_engine::runner::{ExecutionRunner, RunOutcome};
use boss_engine::spawn_flow::{StartWorkerInput, TmuxWorkerHost, WorkerSpawner, start_worker};
use boss_engine::stale_worker_sweep::{
    StaleWorkerReaper, StaleWorkerSweepControls, StaleWorkerSweepTiming, StaleWorkerThresholds,
    TmuxWorkerTerminalInspector, WorkerTerminalInspector, run_one_pass_with_terminal_and_grace,
};
use boss_engine::tmux_adoption::run_adoption_pass;
use boss_engine::work::WorkDb;
use boss_engine::worker_readoption::NoopLiveWorkerConvergence;
use boss_engine::worker_registry::WorkerRegistry;
use boss_engine::worker_setup::WorkerKind;
use boss_protocol::{
    AttachWorkerPaneResult, CreateChoreInput, CreateProductInput, EngineToAppRequest, EngineToAppResponse,
    RequestExecutionInput, WorkerEvent,
};
use boss_tmux::{KillSessionOutcome, Tmux};

const REPAINTING_WORKER: &str = include_str!("fixtures/repainting-worker.sh");
const SHORT_WINDOW: Duration = Duration::from_secs(1);

/// App facade for the production spawn path: it validates that the engine
/// attached a viewer to the tmux-owned worker and acknowledges that attach.
#[derive(Default)]
struct AttachingSpawner {
    registry: WorkerRegistry,
    attached_sessions: Mutex<Vec<String>>,
}

#[async_trait]
impl WorkerSpawner for AttachingSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        _timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        match request {
            EngineToAppRequest::AttachWorkerPane(input) => {
                self.attached_sessions.lock().unwrap().push(input.session_name);
                Ok(EngineToAppResponse::AttachWorkerPane {
                    result: Ok(AttachWorkerPaneResult {}),
                })
            }
            other => panic!("tmux-hosted spawn must attach a viewer, got {other:?}"),
        }
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }
}

/// Fresh engine-local state used after the simulated restart. Adoption must
/// rebuild this state entirely from the durable run and the live tmux session.
#[derive(Default)]
struct RestartSpawner {
    registry: WorkerRegistry,
    live_states: LiveWorkerStateRegistry,
}

#[async_trait]
impl WorkerSpawner for RestartSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        _timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        panic!("tmux adoption must not ask the app to spawn or attach: {request:?}")
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }

    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        Some(&self.live_states)
    }
}

/// The redispatch kick is intentionally inert in this fixture. The test
/// drives `orphan_sweep` directly after the pane has been reaped, avoiding a
/// real cube lease while preserving the production recovery ordering.
struct FixtureRunner;

#[async_trait]
impl ExecutionRunner for FixtureRunner {
    async fn run_execution(
        &self,
        _worker_id: &str,
        _execution: &boss_protocol::WorkExecution,
        _work_item: &boss_protocol::WorkItem,
        _workspace_path: &Path,
        _cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        bail!("the fixture must stop at redispatch, before a replacement worker is launched")
    }
}

/// Records the force-release issued after the token-verified tmux teardown.
#[derive(Default)]
struct FixtureCube {
    released_leases: Mutex<Vec<String>>,
}

#[async_trait]
impl CubeClient for FixtureCube {
    async fn ensure_repo(&self, _origin: &str) -> Result<CubeRepoHandle> {
        bail!("the fixture must not start a replacement cube lease")
    }

    async fn lease_workspace(
        &self,
        _repo_id: &str,
        _task: &str,
        _prefer_workspace_id: Option<&str>,
        _allow_dirty: bool,
        _exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        bail!("the fixture must not start a replacement cube lease")
    }

    async fn create_change(&self, _workspace_path: &Path, _title: &str) -> Result<CubeChangeHandle> {
        bail!("the fixture must not create a replacement cube change")
    }

    async fn goto_workspace(&self, _workspace_path: &Path, _pr: u64) -> Result<()> {
        bail!("the fixture must not reposition a workspace")
    }

    async fn release_workspace(&self, _lease_id: &str) -> Result<()> {
        Ok(())
    }

    async fn workspace_status(&self, _workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        bail!("the fixture does not query cube workspace status")
    }

    async fn heartbeat_lease(&self, _lease_id: &str, _ttl_seconds: Option<u64>) -> Result<()> {
        Ok(())
    }

    async fn force_release_lease(&self, lease_id: &str, _reason: Option<&str>) -> Result<()> {
        self.released_leases.lock().unwrap().push(lease_id.to_owned());
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(Vec::new())
    }
}

/// The production stale sweep delegates pane destruction to this adapter. It
/// uses the real `kill_session_verified` primitive before the sweep releases
/// the pool slot and cube lease.
struct FixtureTmuxReaper {
    tmux: Tmux,
    session_name: String,
    spawn_token: String,
    reaped: Mutex<Vec<String>>,
}

#[async_trait]
impl StaleWorkerReaper for FixtureTmuxReaper {
    async fn reap_worker(&self, execution_id: &str) {
        let outcome = self
            .tmux
            .kill_session_verified(&self.session_name, &self.spawn_token)
            .await
            .expect("token-verified tmux fixture teardown");
        assert_eq!(outcome, KillSessionOutcome::Killed);
        self.reaped.lock().unwrap().push(execution_id.to_owned());
    }
}

fn write_repainting_shell(root: &Path) -> Result<PathBuf> {
    let shell_path = root.join("repainting-worker.sh");
    std::fs::write(&shell_path, REPAINTING_WORKER)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&shell_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&shell_path, permissions)?;
    }
    Ok(shell_path)
}

/// Resolves the sole host executable intentionally declared as data for this
/// target. The test sandbox canonicalizes executable runfiles, so this keeps
/// the production tmux binary available without widening every test's PATH.
fn declared_tmux_binary() -> Result<PathBuf> {
    let test_srcdir = PathBuf::from(std::env::var("TEST_SRCDIR")?);
    let host_tmux_runfiles = std::fs::read_dir(&test_srcdir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("host_tmux"))
        })
        .ok_or_else(|| anyhow!("Bazel did not provide the declared host tmux runfiles"))?;
    let tmux = host_tmux_runfiles.join("tmux");
    if !tmux.is_file() {
        bail!("the declared tmux binary is unavailable at {}", tmux.display());
    }
    Ok(tmux)
}

/// Kills the private tmux server this fixture started, on drop.
///
/// `prepare_server`'s `exit-empty=off` (the fix this target exists to cover)
/// deliberately stops the server from self-terminating once its last
/// session is killed, so without this guard every run of this test leaks a
/// tmux server process and its unlinked socket. Uses a plain synchronous
/// `Command` rather than [`Tmux::kill_server`] so teardown still runs from a
/// panicking assertion's unwind, with no dependency on the tokio runtime
/// still being in a state that can drive an async call.
struct TmuxServerGuard {
    program: PathBuf,
    socket: PathBuf,
}

impl Drop for TmuxServerGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.program)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}

async fn wait_for_repaint(tmux: &Tmux, session_name: &str) -> Result<(i64, String)> {
    let first_activity = tmux
        .display_message(session_name, boss_tmux::DisplayField::WindowActivity)
        .await?
        .trim()
        .parse::<i64>()?;
    for _ in 0..4 {
        tokio::time::sleep(SHORT_WINDOW).await;
        let activity = tmux
            .display_message(session_name, boss_tmux::DisplayField::WindowActivity)
            .await?
            .trim()
            .parse::<i64>()?;
        let captured = tmux.capture_pane(session_name).await?;
        if activity > first_activity && captured.contains("boss-repainting-fixture-title") {
            return Ok((activity, captured));
        }
    }
    Err(anyhow!(
        "fixture did not repaint its tmux window within the short integration window"
    ))
}

#[tokio::test]
async fn production_tmux_recovery_ignores_repaint_and_process_title_then_redispatches() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let home = temp.path().join("home");
    std::fs::create_dir(&home)?;
    let repainting_shell = write_repainting_shell(temp.path())?;
    let _home = boss_engine::driver::test_support::home_override(&home);
    let _shell = boss_engine::driver::test_support::shell_override(&repainting_shell);

    let work_db = Arc::new(WorkDb::open(temp.path().join("state.db"))?);
    let product = work_db.create_product(
        CreateProductInput::builder()
            .name("tmux recovery fixture")
            .repo_remote_url("https://example.invalid/tmux-recovery.git")
            .build(),
    )?;
    let chore = work_db.create_chore(
        CreateChoreInput::builder()
            .product_id(product.id)
            .name("exercise tmux recovery")
            .autostart(true)
            .build(),
    )?;
    let execution =
        work_db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())?;
    work_db.start_execution_run_on_host_with_tmux_hosting(
        &execution.id,
        "worker-1",
        "fixture-repo",
        "fixture-lease",
        "fixture-workspace",
        workspace.to_str().expect("temporary workspace is UTF-8"),
        "local",
        true,
    )?;

    let tmux_socket = temp.path().join("private.tmux.sock");
    let tmux_binary = declared_tmux_binary()?;
    let tmux = Tmux::from_path_with_socket(tmux_binary.clone(), &tmux_socket)?;
    let _tmux_server_guard = TmuxServerGuard {
        program: tmux_binary,
        socket: tmux_socket,
    };
    let session_name = "boss-worker-1-recovery-fixture".to_owned();
    let spawn_store: Arc<dyn boss_engine::spawn_flow::TmuxSpawnStore> = work_db.clone();
    let attaching_spawner = AttachingSpawner::default();
    let started = start_worker(
        &attaching_spawner,
        StartWorkerInput {
            run_id: execution.id.clone(),
            lease_id: "fixture-lease".to_owned(),
            slot_id: 1,
            workspace_path: workspace.clone(),
            events_socket_path: temp.path().join("events.sock"),
            boss_event_path: PathBuf::from("/usr/bin/true"),
            initial_input: "fixture prompt".to_owned(),
            extra_env: Vec::new(),
            title_summary: Some("repainting fixture".to_owned()),
            task_title: Some("exercise tmux recovery".to_owned()),
            work_item_binding: None,
            model: "claude-opus-4-7".to_owned(),
            draft_pr_mode: false,
            execution_kind: "chore_implementation".to_owned(),
            pool: Some("main".to_owned()),
            task_kind: Some("chore".to_owned()),
            worker_kind: WorkerKind::Standard,
            driver: Arc::new(ClaudeDriver),
            tmux_host: Some(TmuxWorkerHost::new(tmux.clone(), spawn_store, session_name.clone())),
            automation_outcome_proposals_seam_enabled: false,
            is_review_supervisor: false,
            is_post_merge_reviewer: false,
        },
        SHORT_WINDOW,
    )
    .await?;
    assert!(started.shell_pid > 0);
    assert_eq!(
        attaching_spawner.attached_sessions.lock().unwrap().as_slice(),
        [session_name.as_str()]
    );
    let spawned_run = work_db
        .tmux_run_for_execution(&execution.id)?
        .expect("production spawn must durably record the private tmux identity");
    assert_eq!(spawned_run.tmux_session_name, session_name);

    let (_, captured) = wait_for_repaint(&tmux, &session_name).await?;
    assert!(captured.contains("boss-repainting-fixture-title"));

    let cube = Arc::new(FixtureCube::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        work_db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FixtureRunner),
    ));
    let events = RecordingDispatchEventSink::new();
    let restart_spawner = RestartSpawner::default();
    let adoption = run_adoption_pass(
        work_db.as_ref(),
        &tmux,
        coordinator.as_ref(),
        &restart_spawner,
        &NoopLiveWorkerConvergence,
        &events,
    )
    .await;
    assert!(adoption.adopted_execution_ids.contains(&execution.id));
    assert_eq!(restart_spawner.live_states.snapshot().len(), 1);

    let inspector = TmuxWorkerTerminalInspector::new(work_db.clone(), tmux.clone(), None);
    let terminal = inspector
        .inspect(&execution.id)
        .await?
        .expect("adopted fixture has terminal identity");
    let boss_engine::stale_worker_sweep::TerminalLiveness::Alive {
        pane_current_command,
        window_activity_epoch_secs,
        ..
    } = terminal
    else {
        bail!("repainting fixture unexpectedly died before stale recovery")
    };
    assert_ne!(pane_current_command.as_deref(), Some("claude"));
    assert!(window_activity_epoch_secs.is_some());

    restart_spawner.live_states.apply_event(
        1,
        &WorkerEvent::PreToolUse {
            session_id: "fixture-session".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
        },
    );
    restart_spawner.live_states.apply_event(
        1,
        &WorkerEvent::PostToolUse {
            session_id: "fixture-session".to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({}),
            tool_response: serde_json::json!({}),
        },
    );
    tokio::time::sleep(SHORT_WINDOW).await;

    let reaper = FixtureTmuxReaper {
        tmux: tmux.clone(),
        session_name: spawned_run.tmux_session_name,
        spawn_token: spawned_run.tmux_spawn_token,
        reaped: Mutex::new(Vec::new()),
    };
    let holds = HoldRegistry::new();
    let outcome = run_one_pass_with_terminal_and_grace(
        work_db.as_ref(),
        &restart_spawner.live_states,
        Some(&inspector),
        coordinator.clone(),
        &events,
        StaleWorkerSweepControls {
            reaper: &reaper,
            hold_registry: &holds,
            cube_client: cube.as_ref(),
        },
        StaleWorkerSweepTiming {
            thresholds: StaleWorkerThresholds {
                stale_threshold_secs: 0,
                auto_reap_threshold_secs: 0,
            },
            startup_grace_secs: 0,
        },
    )
    .await;
    assert_eq!(
        outcome.reaped, 1,
        "repainting/title changes must not veto semantic stale recovery"
    );
    assert_eq!(reaper.reaped.lock().unwrap().as_slice(), [execution.id.as_str()]);
    assert!(
        tmux.list_sessions().await?.is_empty(),
        "token-verified teardown removes the private session"
    );
    assert_eq!(cube.released_leases.lock().unwrap().as_slice(), ["fixture-lease"]);
    assert!(coordinator.worker_pool().has_idle_worker().await);

    // The candidate query intentionally uses a strict timestamp comparison;
    // advance beyond the terminal-write second while retaining a test-local
    // zero-second configured minimum age.
    tokio::time::sleep(SHORT_WINDOW).await;
    assert!(
        work_db.list_orphan_active_candidates(0)?.contains(&chore.id),
        "reaped auto-start chore must become eligible for controlled redispatch"
    );
    let redispatch = run_one_pass_for_item_with_min_age(
        work_db.as_ref(),
        coordinator,
        &events,
        &NoopLiveWorkerConvergence,
        &chore.id,
        0,
    )
    .await;
    assert_eq!(redispatch.redispatched, 1);
    let executions = work_db.list_executions(Some(&chore.id))?;
    assert!(
        executions
            .iter()
            .any(|candidate| candidate.id != execution.id && candidate.status.as_str() == "ready"),
        "orphan recovery must create a fresh ready execution"
    );

    Ok(())
}
