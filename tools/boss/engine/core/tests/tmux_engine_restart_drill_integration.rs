//! Genuine engine-restart drill for tmux-hosted worker survival.
//!
//! This is the acceptance test the tmux-only-hosting work requires: it
//! boots the *real* production `serve()` entry point (the same function
//! `main.rs` calls) as engine #1 against a real private tmux server and a
//! pre-existing tmux-hosted worker process, sends it the real `Shutdown`
//! RPC (the same wire path an operator or `bossctl` uses — not a direct
//! call into `ServerState::shutdown_workers`), confirms via real tmux and a
//! continuously-growing heartbeat file that the worker process survives
//! engine #1's exit, boots a second real `serve()` instance (engine #2)
//! against the same durable state and tmux session (the actual restart),
//! and confirms via the durable dispatch-event timeline and further
//! heartbeat growth that the *same* worker process is re-adopted and keeps
//! making progress — never respawned, never interrupted.
//!
//! Two things are deliberately out of scope, and don't affect what this
//! drill demonstrates: the login shell is substituted for a lightweight
//! heartbeat script (as the existing tmux-recovery fixture already does)
//! so the drill needs no live model credentials, and it drives `serve()`
//! in-process rather than the compiled `boss-engine` binary as a
//! subprocess — `serve()` *is* the whole engine; `main.rs` is only argv
//! parsing and logging setup around it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::{SendToAppError, serve};
use boss_engine::config::{RuntimeConfig, WorkConfig, tmux_socket_path_beside_db};
use boss_engine::driver::ClaudeDriver;
use boss_engine::engine_control::ControlTokenFile;
use boss_engine::spawn_flow::{StartWorkerInput, TmuxSpawnStore, TmuxWorkerHost, WorkerSpawner, start_worker};
use boss_engine::work::WorkDb;
use boss_engine::worker_registry::WorkerRegistry;
use boss_protocol::{
    AttachWorkerPaneResult, CreateChoreInput, CreateProductInput, EngineToAppRequest, EngineToAppResponse,
    FrontendEvent, FrontendRequest, RequestExecutionInput,
};
use boss_tmux::Tmux;

const STILL_WORKING: &str = include_str!("fixtures/still-working.sh");
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const ADOPTION_TIMEOUT: Duration = Duration::from_secs(20);
const GROWTH_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Minimal app facade used only to create the pre-existing worker before
/// either engine exists — mirrors how a prior engine's own dispatch would
/// have spawned it. Neither engine under test ever talks to this facade.
#[derive(Default)]
struct AttachingSpawner {
    registry: WorkerRegistry,
}

#[async_trait]
impl WorkerSpawner for AttachingSpawner {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        _timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        match request {
            EngineToAppRequest::AttachWorkerPane(_) => Ok(EngineToAppResponse::AttachWorkerPane {
                result: Ok(AttachWorkerPaneResult {}),
            }),
            other => panic!("tmux-hosted spawn must attach a viewer, got {other:?}"),
        }
    }

    fn worker_registry(&self) -> &WorkerRegistry {
        &self.registry
    }
}

fn write_still_working_shell(root: &Path) -> Result<PathBuf> {
    let shell_path = root.join("still-working.sh");
    std::fs::write(&shell_path, STILL_WORKING)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&shell_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&shell_path, permissions)?;
    }
    Ok(shell_path)
}

/// Resolves the declared host tmux binary from Bazel runfiles — see
/// `tmux_recovery_integration.rs`'s identical helper for the rationale
/// (the hermetic test sandbox only permits precisely declared executables).
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

/// Kills the private tmux server this drill started, on drop — same
/// rationale as `tmux_recovery_integration.rs`'s guard of the same name.
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

async fn heartbeat_len(path: &Path) -> u64 {
    tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0)
}

/// Poll `path` until its size exceeds `floor`, proving the worker process
/// is genuinely alive and making progress (not merely present).
async fn wait_for_growth(path: &Path, floor: u64, timeout: Duration) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let len = heartbeat_len(path).await;
        if len > floor {
            return Ok(len);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("heartbeat.log never grew past {floor} bytes within {timeout:?} (worker did not make progress)");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll the engine's own durable dispatch-event timeline
/// (`<state_root>/dispatch-events/current.jsonl`) until at least
/// `min_count` `stage` events have been recorded for `execution_id`. Used
/// to detect real boot-time tmux adoption without reaching into engine
/// internals: each `serve()` boot's own startup adoption pass emits a
/// `tmux_adopt` dispatch event for every session it re-attaches.
async fn wait_for_dispatch_event(
    state_root: &Path,
    execution_id: &str,
    stage: &str,
    min_count: usize,
    timeout: Duration,
) -> Result<()> {
    let path = state_root.join("dispatch-events").join("current.jsonl");
    let needle = format!("\"stage\":\"{stage}\"");
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let count = contents
                .lines()
                .filter(|line| line.contains(execution_id) && line.contains(&needle))
                .count();
            if count >= min_count {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "dispatch-events never recorded {min_count} '{stage}' event(s) for {execution_id} within {timeout:?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Send the real `Shutdown` RPC (reading the engine-minted control token
/// off disk, exactly as `bossctl`/the app do) and wait for `serve()` to
/// return. This drives the exact same code path a real operator shutdown
/// or app-triggered engine stop takes — never a direct call into
/// `ServerState::shutdown_workers`.
async fn shutdown_and_join(control_token_path: &Path, engine: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    let raw_token = std::fs::read_to_string(control_token_path)
        .with_context(|| format!("reading control token at {}", control_token_path.display()))?;
    let token_file: ControlTokenFile = serde_json::from_str(&raw_token)?;
    let mut client = BossClient::connect_socket(&token_file.socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::Shutdown {
            token: token_file.token.clone(),
        })
        .await?;
    if !matches!(response, FrontendEvent::ShutdownAccepted) {
        bail!("engine rejected the drill's shutdown rpc: {response:?}");
    }
    tokio::time::timeout(SHUTDOWN_TIMEOUT, engine)
        .await
        .map_err(|_| anyhow!("engine never exited after accepting the shutdown rpc"))??
}

#[tokio::test]
async fn engine_restart_preserves_and_reattaches_a_live_tmux_worker() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let home = temp.path().join("home");
    std::fs::create_dir(&home)?;

    let worker_shell = write_still_working_shell(temp.path())?;
    let _home = boss_engine::driver::test_support::home_override(&home);
    let _shell = boss_engine::driver::test_support::shell_override(&worker_shell);

    let db_path = temp.path().join("state.db");
    let tmux_socket = tmux_socket_path_beside_db(&db_path)?;
    let tmux_binary = declared_tmux_binary()?;

    // The real engine resolves tmux via `which::which("tmux")` (production
    // behaviour — it does not know about Bazel runfiles), so make the
    // declared binary the one `which` finds, the same way a real host
    // would have tmux on PATH.
    let tmux_dir = tmux_binary
        .parent()
        .ok_or_else(|| anyhow!("declared tmux binary has no parent directory"))?;
    let prior_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = tmux_dir.as_os_str().to_owned();
    new_path.push(":");
    new_path.push(&prior_path);
    // SAFETY: this test is the only test function in this binary, so no
    // other test can observe a torn PATH value.
    unsafe { std::env::set_var("PATH", &new_path) };

    let tmux = Tmux::from_path_with_socket(tmux_binary.clone(), &tmux_socket)?;
    let _tmux_server_guard = TmuxServerGuard {
        program: tmux_binary,
        socket: tmux_socket.clone(),
    };

    // Pre-seed the durable tmux-hosted worker exactly as a prior engine's
    // own dispatch would have: a real private tmux session running a real
    // OS process, with `work_runs.tmux_hosted = 1` recorded durably.
    let session_name = "boss-worker-1-restart-drill".to_owned();
    let execution_id = {
        let work_db = Arc::new(WorkDb::open(db_path.clone())?);
        let product = work_db.create_product(
            CreateProductInput::builder()
                .name("restart drill")
                .repo_remote_url("https://example.invalid/restart-drill.git")
                .build(),
        )?;
        let chore = work_db.create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("exercise the engine-restart drill")
                .autostart(true)
                .build(),
        )?;
        let execution =
            work_db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())?;
        work_db.start_execution_run_on_host_with_tmux_hosting(
            &execution.id,
            "worker-1",
            "drill-repo",
            "drill-lease",
            "drill-workspace",
            workspace.to_str().expect("temporary workspace is UTF-8"),
            "local",
            true,
        )?;

        let spawn_store: Arc<dyn TmuxSpawnStore> = work_db.clone();
        let spawner = AttachingSpawner::default();
        let started = start_worker(
            &spawner,
            StartWorkerInput::builder()
                .run_id(execution.id.clone())
                .lease_id("drill-lease")
                .slot_id(1)
                .workspace_path(workspace.clone())
                .events_socket_path(temp.path().join("worker-events.sock"))
                .boss_event_path(PathBuf::from("/usr/bin/true"))
                .initial_input("drill prompt")
                .title_summary("engine-restart drill")
                .task_title("exercise the engine-restart drill")
                .model("claude-opus-4-7")
                .execution_kind("chore_implementation")
                .pool("main")
                .task_kind("chore")
                .driver(Arc::new(ClaudeDriver))
                .tmux_host(TmuxWorkerHost::new(tmux.clone(), spawn_store, session_name.clone()))
                .build(),
            Duration::from_secs(5),
        )
        .await?;
        assert!(started.shell_pid > 0, "worker must have a real OS process");

        execution.id
        // `work_db` (and its Arc clone held by `spawn_store`) is dropped
        // here, before either engine opens its own connection to the same
        // sqlite file.
    };

    let heartbeat_path = workspace.join("heartbeat.log");
    let initial_len = wait_for_growth(&heartbeat_path, 0, GROWTH_TIMEOUT).await?;
    assert!(
        initial_len > 0,
        "worker must be genuinely running before the drill starts"
    );

    // --- Engine #1: real boot, real adoption of the pre-existing worker ---
    let socket_path = temp.path().join("engine.sock");
    let control_token_path = temp.path().join("engine.control-token");
    let cfg1 = Arc::new(RuntimeConfig::from_parts(
        WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path.clone())
            .build(),
        None,
    ));
    let engine1 = {
        let sock = socket_path.clone();
        let token_path = control_token_path.clone();
        tokio::spawn(async move { serve(cfg1, sock, None, None, Some(token_path), None).await })
    };
    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        engine1.abort();
        bail!("engine #1 never bound its socket");
    }
    wait_for_dispatch_event(temp.path(), &execution_id, "tmux_adopt", 1, ADOPTION_TIMEOUT).await?;

    // --- Real shutdown: the production `Shutdown` RPC, not a direct call
    // into `ServerState::shutdown_workers`. ---
    shutdown_and_join(&control_token_path, engine1).await?;

    // The tmux session and its worker process must survive engine #1's
    // exit — real tmux evidence and continued file growth, not a mocked
    // assertion.
    let sessions = tmux.list_sessions().await?;
    assert!(
        sessions.iter().any(|session| session.name == session_name),
        "tmux-hosted session must survive engine #1's shutdown, got {sessions:?}"
    );
    let len_after_shutdown = wait_for_growth(&heartbeat_path, initial_len, GROWTH_TIMEOUT).await?;

    // --- Engine #2: the actual restart, against the same durable state
    // and the same still-running tmux session. ---
    let cfg2 = Arc::new(RuntimeConfig::from_parts(
        WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path.clone())
            .build(),
        None,
    ));
    let engine2 = {
        let sock = socket_path.clone();
        let token_path = control_token_path.clone();
        tokio::spawn(async move { serve(cfg2, sock, None, None, Some(token_path), None).await })
    };
    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        engine2.abort();
        bail!("engine #2 never bound its socket");
    }
    wait_for_dispatch_event(temp.path(), &execution_id, "tmux_adopt", 2, ADOPTION_TIMEOUT).await?;

    // The worker made progress the whole time — same OS process, no
    // respawn, across both the shutdown and the restart.
    let len_after_restart = wait_for_growth(&heartbeat_path, len_after_shutdown, GROWTH_TIMEOUT).await?;
    assert!(len_after_restart > len_after_shutdown);

    shutdown_and_join(&control_token_path, engine2).await?;

    // Durable identity survived both the shutdown and the restart intact —
    // checked only now, after both engines have fully exited, so this
    // connection never contends with either engine's own.
    let db = WorkDb::open(db_path.clone())?;
    let identity = db
        .tmux_identity_for_execution(&execution_id)?
        .expect("tmux identity must survive the drill");
    assert_eq!(identity.session_name, session_name);
    let adoptable = db.list_adoptable_tmux_runs()?;
    assert!(
        adoptable.iter().any(|handle| handle.execution_id == execution_id),
        "surviving tmux identity must still match the adoption predicate, got {adoptable:?}"
    );

    Ok(())
}
