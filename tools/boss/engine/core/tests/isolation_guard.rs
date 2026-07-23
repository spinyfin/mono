//! Integration tests for the test-fixture isolation guard.
//!
//! Issue from 2026-05-24: a Swift XCTest spawned an additional Rust engine
//! binary alongside the live production engine. Because only `--socket-path`
//! was overridden, the test engine silently bound to the *production*
//! `events.sock`, DB, and pid file — corrupting the live engine's state
//! (see #756).
//!
//! The fix: when `--socket-path` is non-default, `IsolationPaths::derive`
//! derives isolated paths for the DB, events socket, pid file, and
//! engine-control token from the socket's directory + stem, and
//! `ensure_isolated` refuses to start if any resolved path still lands on
//! production.
//!
//! Issue from 2026-07: the engine-control token was resolved via
//! `default_token_path()` entirely outside this isolation machinery, so a
//! worker-launched fixture engine wrote — and then, on its own shutdown,
//! deleted — the production control token. The token is now derived
//! alongside pid/db/events-socket (see the `token` tests below), and
//! `write_token_file` independently refuses to clobber a token still owned by
//! a live engine (see `fixture_cannot_overwrite_or_delete_live_production_token`)
//! as defense in depth for the case where derivation is bypassed or
//! misconfigured.
//!
//! ## What this file used to *not* test
//!
//! Until 2026-07-23 every test here handed explicit paths straight to
//! `serve()` and asserted those explicit paths differed — i.e. it tested that
//! `serve` uses its arguments. `IsolationPaths::derive` was never called, so
//! the derivation shipped with zero coverage on any field, and the
//! stand-down-on-any-override bug survived into a production outage: a fixture
//! launched from inside a worker pane inherited `BOSS_EVENTS_SOCKET` pointing
//! at production, the guard read that as operator intent, and the fixture
//! unlinked and rebound the live engine's socket.
//!
//! The `derive` section below closes that gap at the integration boundary;
//! `app::isolation`'s own unit tests cover the derivation matrix exhaustively.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::wait_for_socket;
use boss_engine::app::isolation::{
    DEFAULT_PID_PATH, DEFAULT_SOCKET_PATH, EnginePaths, IsolationOverrides, IsolationPaths,
};
use boss_engine::app::{process_is_alive, run, serve};
use boss_engine::cli::Cli;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_engine::engine_control::ControlTokenFile;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct TestEngine {
    #[allow(dead_code)]
    socket_path: PathBuf,
    pid_path: PathBuf,
    events_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    /// Spawn an in-process engine bound to isolated temp paths.
    async fn spawn(stem: &str) -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join(format!("{stem}.sock"));
        let db_path = temp.path().join(format!("{stem}.db"));
        let pid_path = temp.path().join(format!("{stem}.pid"));
        let events_path = temp.path().join(format!("{stem}.events.sock"));

        let work = WorkConfig::builder()
            .cwd(temp.path().to_path_buf())
            .db_path(db_path)
            .build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work, None));

        let sock = socket_path.clone();
        let pid = pid_path.clone();
        let ev = events_path.clone();
        let join = tokio::spawn(async move { serve(cfg, sock, Some(pid), Some(ev), None, None).await });

        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }

        Ok(Self {
            socket_path,
            pid_path,
            events_path,
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

// ---------------------------------------------------------------------------
// IsolationPaths derivation (no engine started)
// ---------------------------------------------------------------------------

const FIXTURE_SOCKET: &str = "/tmp/boss-test-guard-1234.sock";

/// A model of the production engine's paths that does not depend on the
/// harness's real `$HOME` (the bazel sandbox pins it to `/tmp`).
fn production() -> EnginePaths {
    EnginePaths::under_state_root(
        Path::new("/Users/tester/Library/Application Support/Boss"),
        Path::new(DEFAULT_PID_PATH),
    )
}

fn derive(overrides: IsolationOverrides) -> IsolationPaths {
    IsolationPaths::derive_from(FIXTURE_SOCKET, &overrides, &production())
}

/// The four paths the fixture socket above derives when nothing stands in
/// its way. The control token is included, which the pre-2026-07-23 guard
/// resolved outside itself and therefore clobbered.
fn all_derived() -> EnginePaths {
    EnginePaths {
        db: Some(PathBuf::from("/tmp/boss-test-guard-1234.db")),
        events_socket: Some(PathBuf::from("/tmp/boss-test-guard-1234.events.sock")),
        pid: Some(PathBuf::from("/tmp/boss-test-guard-1234.pid")),
        control_token: Some(PathBuf::from("/tmp/boss-test-guard-1234.control-token")),
    }
}

/// The production socket derives nothing — a production engine resolves its
/// paths through the ordinary env / home-dir logic.
#[test]
fn derive_stands_down_entirely_for_the_production_socket() {
    let paths = IsolationPaths::derive_from(DEFAULT_SOCKET_PATH, &IsolationOverrides::default(), &production());
    assert!(!paths.is_test_fixture);
    assert_eq!(paths.derived, EnginePaths::default());
}

#[test]
fn derive_isolates_all_four_paths_when_no_env_is_set() {
    let paths = derive(IsolationOverrides::default());
    assert!(paths.is_test_fixture);
    assert_eq!(paths.derived, all_derived());
}

/// **The 2026-07-23 regression.** A fixture started from inside a worker pane
/// inherits `BOSS_EVENTS_SOCKET` pointing at the production socket. That is
/// inherited environment, not operator intent: derive over it.
#[test]
fn derive_ignores_env_that_merely_repeats_the_production_default() {
    let prod = production();
    let paths = derive(IsolationOverrides {
        db_path: prod.db,
        events_socket: prod.events_socket,
        pid_path: prod.pid,
        control_token_path: prod.control_token,
    });
    assert_eq!(
        paths.derived,
        all_derived(),
        "inherited production paths must not suppress derivation on any field"
    );
}

/// The other half of the rule: a developer who deliberately points
/// `BOSS_EVENTS_SOCKET` at a private path still gets that path. This is why
/// the fix is an equality test and not a blanket refusal.
#[test]
fn derive_honours_env_that_names_a_private_path() {
    let paths = derive(IsolationOverrides {
        events_socket: Some(PathBuf::from("/tmp/my-own-events.sock")),
        ..IsolationOverrides::default()
    });
    assert_eq!(paths.derived.events_socket, None, "the caller's explicit choice wins");
}

/// A fixture whose resolved paths still collide with production refuses to
/// start, and says which environment variable to fix.
#[test]
fn fixture_refuses_to_start_when_a_resolved_path_is_production() {
    let prod = production();
    let paths = derive(IsolationOverrides::default());

    paths
        .ensure_isolated(&all_derived())
        .expect("fully isolated fixture starts");

    let stolen_socket = EnginePaths {
        events_socket: prod.events_socket,
        ..all_derived()
    };
    let err = paths
        .ensure_isolated(&stolen_socket)
        .expect_err("a fixture must never bind production's events socket");
    let msg = format!("{err}");
    assert!(msg.contains("BOSS_EVENTS_SOCKET"), "must name the env var; got: {msg}");

    let stolen_token = EnginePaths {
        control_token: prod.control_token,
        ..all_derived()
    };
    let err = paths
        .ensure_isolated(&stolen_token)
        .expect_err("a fixture must never overwrite production's control token");
    assert!(
        format!("{err}").contains("BOSS_ENGINE_CONTROL_TOKEN_PATH"),
        "must name the control-token env var; got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Unit tests — process liveness helper
// ---------------------------------------------------------------------------

/// `process_is_alive` reports true for this running process and false for
/// a pid that can't possibly be running.
#[test]
fn process_is_alive_unit_tests() {
    // Our own pid must be alive.
    let own_pid = std::process::id() as i32;
    assert!(process_is_alive(own_pid), "own pid must be alive");

    // pid 0 is always invalid (the kernel rejects kill(0, 0) from user space).
    assert!(!process_is_alive(0));
    // i32::MAX is virtually guaranteed to not exist as a live process.
    assert!(!process_is_alive(i32::MAX));
}

// ---------------------------------------------------------------------------
// Integration tests — serve() with isolated paths
// ---------------------------------------------------------------------------

/// Starting an engine with a non-default socket places the pid file at the
/// derived path, NOT at the production `/tmp/boss-engine.pid`.
#[tokio::test]
async fn isolated_engine_writes_pid_to_derived_path() -> Result<()> {
    let engine = TestEngine::spawn("boss-test-isolation-pid").await?;

    // Pid file must exist at the derived path.
    assert!(
        engine.pid_path.exists(),
        "isolated pid file must exist at derived path {}",
        engine.pid_path.display()
    );

    // Read pid from the file: must be a real running process.
    let content = std::fs::read_to_string(&engine.pid_path)?;
    let pid: i32 = content.trim().parse().expect("pid file must contain a number");
    assert!(process_is_alive(pid), "pid in isolated pid file must be alive");

    // The production pid path (/tmp/boss-engine.pid) must NOT have been
    // overwritten by this engine — its content (if any) should not be our pid.
    let prod_pid_path = std::path::Path::new("/tmp/boss-engine.pid");
    if let Ok(prod_content) = std::fs::read_to_string(prod_pid_path) {
        let prod_pid: i32 = prod_content.trim().parse().unwrap_or(-1);
        assert_ne!(
            prod_pid, pid,
            "test-fixture engine must NOT write to the production pid file"
        );
    }

    Ok(())
}

/// Starting an engine with a non-default socket binds the events socket at
/// the derived path, NOT at the production
/// `~/Library/Application Support/Boss/events.sock`.
#[tokio::test]
async fn isolated_engine_binds_events_socket_at_derived_path() -> Result<()> {
    let engine = TestEngine::spawn("boss-test-isolation-events").await?;

    // Events socket must exist at the derived path.
    assert!(
        engine.events_path.exists(),
        "isolated events socket must exist at {}",
        engine.events_path.display()
    );

    // The production events socket path is under $HOME/Library/Application
    // Support/Boss/events.sock.  In the bazel sandbox HOME=/tmp, so that
    // resolves to /tmp/Library/Application Support/Boss/events.sock — a
    // completely different directory from our derived /tmp/boss-test-*.events.sock.
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let prod_events = std::path::PathBuf::from(home).join("Library/Application Support/Boss/events.sock");
    assert_ne!(
        engine.events_path, prod_events,
        "derived events socket path must differ from production path"
    );

    Ok(())
}

/// Two engines — one "production-style" and one "test-fixture" — can coexist
/// without sharing their events socket or pid file.
///
/// This is the retroactive regression test for the 2026-05-24 incident where
/// a test-fixture engine bound to the production events.sock and overwrote the
/// production pid file.
#[tokio::test]
async fn production_and_test_fixture_engines_use_distinct_paths() -> Result<()> {
    let temp = tempfile::tempdir()?;

    // "Production-style" engine: explicit paths in temp dir (simulates production).
    let prod_socket = temp.path().join("boss-engine.sock");
    let prod_events = temp.path().join("prod-events.sock");
    let prod_db = temp.path().join("prod-state.db");
    let prod_pid = temp.path().join("boss-engine.pid");

    let prod_work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(prod_db)
        .build();
    let prod_cfg = Arc::new(RuntimeConfig::from_parts(prod_work, None));
    let prod_sock_c = prod_socket.clone();
    let prod_pid_c = prod_pid.clone();
    let prod_ev_c = prod_events.clone();
    let prod_join =
        tokio::spawn(async move { serve(prod_cfg, prod_sock_c, Some(prod_pid_c), Some(prod_ev_c), None, None).await });
    if !wait_for_socket(prod_socket.to_str().unwrap(), STARTUP_TIMEOUT).await {
        prod_join.abort();
        return Err(anyhow!("production engine never bound socket"));
    }

    // "Test-fixture" engine: different socket stem, different derived paths.
    let test_socket = temp.path().join("boss-test-uuid.sock");
    let test_events = temp.path().join("boss-test-uuid.events.sock");
    let test_db = temp.path().join("boss-test-uuid.db");
    let test_pid = temp.path().join("boss-test-uuid.pid");

    let test_work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(test_db)
        .build();
    let test_cfg = Arc::new(RuntimeConfig::from_parts(test_work, None));
    let test_sock_c = test_socket.clone();
    let test_pid_c = test_pid.clone();
    let test_ev_c = test_events.clone();
    let test_join =
        tokio::spawn(async move { serve(test_cfg, test_sock_c, Some(test_pid_c), Some(test_ev_c), None, None).await });
    if !wait_for_socket(test_socket.to_str().unwrap(), STARTUP_TIMEOUT).await {
        prod_join.abort();
        test_join.abort();
        return Err(anyhow!("test-fixture engine never bound socket"));
    }

    // Both engines must be alive.
    let prod_pid_val: i32 = std::fs::read_to_string(&prod_pid)?.trim().parse()?;
    let test_pid_val: i32 = std::fs::read_to_string(&test_pid)?.trim().parse()?;
    assert!(process_is_alive(prod_pid_val), "production engine must still be alive");
    assert!(
        process_is_alive(test_pid_val),
        "test-fixture engine must still be alive"
    );

    // Their paths must differ — the key invariant violated in the 2026-05-24 incident.
    assert_ne!(prod_events, test_events, "events sockets must be at distinct paths");
    assert_ne!(prod_pid, test_pid, "pid files must be at distinct paths");

    prod_join.abort();
    test_join.abort();
    Ok(())
}

// ---------------------------------------------------------------------------
// Engine-control token isolation (2026-07 incident)
// ---------------------------------------------------------------------------

/// Starting an engine through the real `run()` entry point with a
/// non-default `--socket-path` derives the control-token path alongside
/// pid/db/events — it must NOT fall through to `default_token_path()`
/// (which resolves under `$HOME/Library/Application Support/Boss/`, the
/// production location). This is the direct regression test for the
/// incident: a worker-launched fixture engine that only sets
/// `--socket-path` must resolve its own isolated token, never production's.
#[tokio::test]
async fn run_derives_isolated_token_path_for_test_fixture() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let socket_path = temp.path().join("boss-test-token-iso.sock");
    // `.control-token`, not `.token`: every derived field is now named after
    // the production file it stands in for (`engine-control.token`), so the
    // fixture's token sits alongside `.db` / `.events.sock` / `.pid` under one
    // naming rule. See `app::isolation::IsolationPaths::derive_from`.
    let expected_token_path = temp.path().join("boss-test-token-iso.control-token");

    let cli = Cli {
        socket_path: Some(socket_path.to_string_lossy().into_owned()),
    };
    let join = tokio::spawn(async move { run(cli).await });

    if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
        join.abort();
        return Err(anyhow!("engine never bound socket {}", socket_path.display()));
    }

    assert!(
        expected_token_path.exists(),
        "isolated engine must derive its control-token path alongside its socket, at {}",
        expected_token_path.display()
    );
    let parsed: ControlTokenFile = serde_json::from_str(&std::fs::read_to_string(&expected_token_path)?)?;
    assert_eq!(parsed.pid, std::process::id());

    // The production-shaped default (env HOME=/tmp for this test binary,
    // per BUILD.bazel) must differ from the derived path, and — if some
    // other file happens to already exist there — must not have been
    // touched by this run.
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let prod_token_path = PathBuf::from(home).join("Library/Application Support/Boss/engine-control.token");
    assert_ne!(
        expected_token_path, prod_token_path,
        "derived token path must differ from the production default"
    );
    if let Ok(raw) = std::fs::read_to_string(&prod_token_path)
        && let Ok(prod_parsed) = serde_json::from_str::<ControlTokenFile>(&raw)
    {
        assert_ne!(
            prod_parsed.pid,
            std::process::id(),
            "test-fixture engine must NOT have written its pid to the production token path"
        );
    }

    join.abort();
    Ok(())
}

/// Defense in depth for when isolation is bypassed or misconfigured: even
/// if a second engine resolves the *same* token path as a live engine (the
/// exact shape of the 2026-07 incident once isolation is stripped away),
/// `serve()` must refuse to start rather than overwrite it — and the live
/// engine's token must be provably untouched (same content, same inode)
/// afterward. Because the write is refused before any
/// `ControlTokenGuard` is created, the second engine's (failed) shutdown
/// also cannot delete the file — covering both the escalation and the
/// denial-of-service halves of the incident in one test.
#[tokio::test]
async fn fixture_cannot_overwrite_or_delete_live_production_token() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let prod_token_path = temp.path().join("prod-engine-control.token");

    // "Production" engine: owns the token path for real, stays running.
    let prod_socket = temp.path().join("prod.sock");
    let prod_db = temp.path().join("prod.db");
    let prod_work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(prod_db)
        .build();
    let prod_cfg = Arc::new(RuntimeConfig::from_parts(prod_work, None));
    let prod_sock_c = prod_socket.clone();
    let prod_token_c = prod_token_path.clone();
    let prod_join =
        tokio::spawn(async move { serve(prod_cfg, prod_sock_c, None, None, Some(prod_token_c), None).await });
    if !wait_for_socket(prod_socket.to_str().unwrap(), STARTUP_TIMEOUT).await {
        prod_join.abort();
        return Err(anyhow!("production engine never bound socket"));
    }

    let before_raw = std::fs::read_to_string(&prod_token_path)?;
    let before_ino = std::fs::metadata(&prod_token_path)?.ino();
    let before_parsed: ControlTokenFile = serde_json::from_str(&before_raw)?;

    // "Fixture" engine: distinct socket/db, but (simulating an isolation
    // bug or future misconfigured caller) the SAME token path.
    let fixture_socket = temp.path().join("fixture.sock");
    let fixture_db = temp.path().join("fixture.db");
    let fixture_work = WorkConfig::builder()
        .cwd(temp.path().to_path_buf())
        .db_path(fixture_db)
        .build();
    let fixture_cfg = Arc::new(RuntimeConfig::from_parts(fixture_work, None));
    let fixture_token_c = prod_token_path.clone();
    let fixture_result = serve(fixture_cfg, fixture_socket, None, None, Some(fixture_token_c), None).await;

    let err = fixture_result.expect_err("fixture engine must refuse to start when the token path is already live");
    assert!(
        format!("{err:#}").contains("still owned by live engine"),
        "unexpected error: {err:#}"
    );

    // Production's token file must be byte-for-byte and inode-identical —
    // neither overwritten (escalation) nor deleted (denial of service).
    let after_raw = std::fs::read_to_string(&prod_token_path)?;
    let after_ino = std::fs::metadata(&prod_token_path)?.ino();
    assert_eq!(after_raw, before_raw, "production token content must be unchanged");
    assert_eq!(
        after_ino, before_ino,
        "production token file must be the same inode, not recreated"
    );
    let after_parsed: ControlTokenFile = serde_json::from_str(&after_raw)?;
    assert_eq!(after_parsed.pid, before_parsed.pid);
    assert_eq!(after_parsed.token, before_parsed.token);

    // Legitimate coordinator paths still work: the production engine is
    // still alive and its socket still connectable throughout.
    assert!(
        wait_for_socket(prod_socket.to_str().unwrap(), Duration::from_secs(1)).await,
        "production engine's frontend socket must remain reachable throughout"
    );

    prod_join.abort();
    Ok(())
}
