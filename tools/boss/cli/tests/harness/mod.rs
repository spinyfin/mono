//! Shared in-process engine harness for the `boss` CLI integration tests
//! that need a live engine.
//!
//! `TestEngine::spawn` starts the engine's `serve` loop on a temp Unix socket
//! backed by a temp SQLite db; `socket_str()` exposes the wire path to pass to
//! the `boss` binary and `db()` opens the same db for tests that seed rows
//! directly. This is its own `rust_library` (testonly), depended on by every
//! engine-backed integration test and used via `use harness::...`. Because
//! it's a library crate, `pub` items that only some dependents use (e.g.
//! `db()`) are not flagged as dead code the way they would be if this were
//! compiled directly into each test binary.
//!
//! The subprocess-driving helpers (`boss_binary`, `run_boss`, …) live in the
//! sibling `common` library.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::work::WorkDb;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct TestEngine {
    socket_path: PathBuf,
    db_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    pub async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = temp.path().join("state.db");
        let work_config = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path.clone())
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }
        Ok(Self {
            socket_path,
            db_path,
            _temp: temp,
            join,
        })
    }

    pub fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }

    pub fn db(&self) -> Result<WorkDb> {
        WorkDb::open(self.db_path.clone())
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}
