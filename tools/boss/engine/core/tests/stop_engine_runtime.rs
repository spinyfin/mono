//! Regression test for #720: `boss_client::stop_engine` must not panic
//! when called from within a tokio runtime.
//!
//! Pre-fix shape: `stop_engine` was a synchronous function whose
//! happy path built a `new_current_thread` tokio runtime and called
//! `block_on` on it. Every real caller in the CLI lives under
//! `#[tokio::main]`, so `block_on` panicked with "Cannot start a
//! runtime from within a runtime", the documented SIGTERM fallback
//! never ran, and `boss engine stop` exited 101 leaving the engine
//! alive.
//!
//! Post-fix shape: `stop_engine` is `async`, awaits the shutdown
//! RPC on the caller's runtime, and reaches the SIGTERM fallback if
//! the RPC fails.
//!
//! Coverage strategy: boot a real engine in-process (so the
//! shutdown RPC has a real socket + token to talk to), deliberately
//! leave its PID file absent, and call `stop_engine(...).await` from
//! within a `#[tokio::test]`. This pins both the nested-runtime fix
//! and socket-first shutdown under the incident's missing-PID shape.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    token_path: PathBuf,
    pid_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let token_path = temp.path().join("engine-control.token");
        let pid_path = temp.path().join("engine.pid");

        let work_config = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path)
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let token_for_serve = token_path.clone();
        let join =
            tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, Some(token_for_serve), None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }

        Ok(Self {
            socket_path,
            token_path,
            pid_path,
            _temp: temp,
            join,
        })
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

/// Regression for #720. Pre-fix this `.await` panics inside the
/// nested `block_on` that `stop_engine` used to build; post-fix it
/// completes cleanly.
#[tokio::test]
async fn stop_engine_from_tokio_runtime_completes_via_rpc() -> Result<()> {
    let engine = TestEngine::spawn().await?;

    let primary_socket = engine.socket_path.with_file_name("state-root.sock");
    let mut discovery = boss_client::Discovery::from_env(Some(primary_socket.to_str().unwrap()))?;
    discovery.legacy_socket_path = Some(engine.socket_path.to_string_lossy().into_owned());
    discovery.legacy_pid_file_path = Some(engine.pid_path.to_string_lossy().into_owned());
    discovery.control_token_path = engine.token_path.clone();
    discovery.autostart = false;
    let result = boss_client::stop_engine(&discovery).await;

    let outcome = result.map_err(|e| anyhow!("stop_engine returned Err: {e:#}"))?;
    assert_eq!(outcome, boss_client::EngineStopOutcome::Stopped);

    // The shutdown RPC took the happy path, so the engine should
    // be tearing down its accept loop. The socket goes away within
    // the shutdown_workers grace window + the 50ms response-defer.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut socket_closed = false;
    while std::time::Instant::now() < deadline {
        if !boss_client::engine_socket_reachable(engine.socket_path.to_str().unwrap()).await {
            socket_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(socket_closed, "engine should have closed its socket after stop_engine");

    // The PID file was absent for the entire operation. Reaching this
    // assertion proves shutdown did not depend on it.
    assert!(
        !engine.pid_path.exists(),
        "stop_engine must not recreate the missing PID file"
    );

    Ok(())
}
