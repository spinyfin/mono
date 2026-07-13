//! Shared `TestEngine` harness for the engine/core integration tests.
//!
//! `shutdown_rpc`, `control_verbs`, `work_crud`, and `comments_crud` each spin
//! up a real in-process engine bound to a temp socket and drive it through the
//! frontend socket. They used to carry near-identical copies of this setup;
//! this module is the single source of truth. Each integration test is its own
//! `rust_test` target, so this file is listed in the `srcs` of every target and
//! pulled in via `mod common;` (mirroring the `watcher_support` module).
//!
//! The small per-test variations are handled by [`TestEngineOptions`] rather
//! than by collapsing them away:
//!   - `on_disk_db` backs the engine with an on-disk SQLite DB (for tests that
//!     reopen the DB out-of-band or read the dispatch log under the state root)
//!     instead of the in-memory default.
//!   - `with_control_token` arms the token-authenticated `Shutdown` RPC and
//!     exposes the token path via [`TestEngine::read_token`].

// Not every consumer touches every field, accessor, or option, and each
// integration test binary compiles this file independently; suppress dead-code
// noise rather than gate individual items per crate.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::engine_control::ControlTokenFile;

/// Budget for the engine to bind its socket. linux-amd64 CI runners run ~6-7x
/// slower than macOS dev boxes; under concurrent test load the first batch of
/// tests blocks on the `binary_fingerprint` `OnceLock` in `build_info::init()`,
/// so 5 s is too tight. 30 s gives headroom for cold starts.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Knobs for the small per-test variations on the shared harness.
#[derive(Default)]
pub struct TestEngineOptions {
    /// Back the engine with an on-disk SQLite DB (`<tempdir>/state.db`) instead
    /// of the in-memory default. Tests that reopen the DB out-of-band
    /// (`WorkDb::open`) or read the dispatch log under the state root need this.
    pub on_disk_db: bool,
    /// Arm the token-authenticated `Shutdown` RPC by passing a control-token
    /// path to `serve`. The path is exposed via `token_path` / `read_token`.
    pub with_control_token: bool,
}

/// An in-process engine bound to a temp socket, torn down on drop.
pub struct TestEngine {
    pub socket_path: PathBuf,
    /// Path passed to `WorkConfig::db_path`. `:memory:` unless
    /// [`TestEngineOptions::on_disk_db`] was set.
    pub db_path: PathBuf,
    /// Control-token path, present only when `with_control_token` was set.
    pub token_path: Option<PathBuf>,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    /// Spawn with the default configuration: in-memory DB, no control token.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_with(TestEngineOptions::default()).await
    }

    /// Spawn with explicit options.
    pub async fn spawn_with(opts: TestEngineOptions) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let db_path = if opts.on_disk_db {
            temp.path().join("state.db")
        } else {
            PathBuf::from(":memory:")
        };
        let token_path = opts
            .with_control_token
            .then(|| temp.path().join("engine-control.token"));

        let work_config = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path.clone())
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));

        let socket_for_serve = socket_path.clone();
        let token_for_serve = token_path.clone();
        let join = tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, token_for_serve, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }

        Ok(Self {
            socket_path,
            db_path,
            token_path,
            _temp: temp,
            join,
        })
    }

    pub fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }

    /// Directory that backs the engine's state (parent of the on-disk DB).
    /// Only meaningful when spawned with [`TestEngineOptions::on_disk_db`].
    pub fn state_root(&self) -> PathBuf {
        self.db_path
            .parent()
            .expect("db path has parent in tempdir")
            .to_path_buf()
    }

    /// Parse the control-token file written by the engine. Requires spawning
    /// with [`TestEngineOptions::with_control_token`].
    pub fn read_token(&self) -> Result<ControlTokenFile> {
        let path = self
            .token_path
            .as_ref()
            .ok_or_else(|| anyhow!("engine spawned without a control token"))?;
        let raw = std::fs::read_to_string(path)?;
        let parsed: ControlTokenFile = serde_json::from_str(&raw)?;
        Ok(parsed)
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}
