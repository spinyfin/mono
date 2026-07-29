//! Engine events socket — accepts connections from `boss-event` shims
//! running inside leased worker workspaces, looks up the connecting
//! peer's pid via `LOCAL_PEERPID`, decodes the JSON hook payload via
//! [`boss_protocol::normalize_hook_event`], and produces typed
//! [`IncomingHookEvent`]s annotated with the peer pid and (when the
//! peer's process tree is registered with [`crate::worker_registry`])
//! the matching `run_id`.
//!
//! Cross-platform: macOS uses `LOCAL_PEERPID`, Linux uses `SO_PEERCRED`.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use boss_event_bus::{Event, EventBus};
use boss_protocol::{NormalizeError, WorkerEvent};
use thiserror::Error;

use crate::driver::{AgentDriver, DriverRegistry, TurnEnd};
use crate::work::WorkDb;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};

/// `level` for `getsockopt(SOL_LOCAL, LOCAL_PEERPID)` on macOS.
#[cfg(target_os = "macos")]
const SOL_LOCAL: libc::c_int = 0;
/// `optname` for the LOCAL_PEERPID getsockopt on macOS.
#[cfg(target_os = "macos")]
const LOCAL_PEERPID: libc::c_int = 0x002;

/// One hook event after peer-pid lookup, payload extraction, and
/// normalization.
///
/// `peer_pid` is best-effort: the peer-pid lookup may return an error
/// once the peer has closed its end (e.g. `ENOTCONN` on macOS), and
/// the shim closes immediately after writing. Callers that need a guaranteed pid must
/// look it up synchronously right after `accept()` (before any async
/// yield) and not rely on `peer_pid` alone for security decisions —
/// the lease registry is the authoritative source.
///
/// `run_id` is extracted from the `_boss_run_id` field in the hook
/// payload, which the event-shim embeds whenever `BOSS_RUN_ID` is set
/// in its environment. The worker-spawn flow always sets this.
///
/// `transcript_path` comes from the run's driver via
/// [`crate::driver::AgentDriver::transcript_path_for_session`] (the Claude
/// driver reads its `transcript_path` field, stamped on every hook payload
/// as `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`). We surface it
/// here so the engine can persist it on `WorkRun` the first time we see it;
/// the live-status summarizer loop reads that row to know which file to
/// tail. Without this, `transcript_path` stays NULL forever and the
/// summarizer never gets past its "no transcript path yet" early-out.
///
/// The turn boundary (`Capability::TurnBoundary`) is resolved here too, once,
/// by asking the run's driver whether the decoded event ends a turn — see
/// [`Self::resolve`]. Every downstream turn-boundary consumer reads
/// [`Self::turn_boundary`] instead of matching [`WorkerEvent::Stop`], so
/// "what counts as a turn ending" is the driver's answer rather than a
/// Claude-shaped assumption baked into the dispatchers.
#[derive(Debug, Clone)]
pub struct IncomingHookEvent {
    pub peer_pid: Option<libc::pid_t>,
    pub run_id: Option<String>,
    pub transcript_path: Option<String>,
    pub event: WorkerEvent,
    driver_resolution: DriverHookResolution,
}

#[derive(Debug, Clone)]
struct DriverHookResolution {
    /// Deliberately private: the only way to populate it is
    /// [`IncomingHookEvent::resolve`], which derives it from a driver. A
    /// boundary that could be hand-set at a construction site is a boundary
    /// the driver seam does not actually own.
    turn_boundary: Option<TurnEnd>,
    /// Driver-owned root the transcript path must remain beneath, resolved at
    /// the same seam as the turn boundary. `Err` is retained rather than
    /// degraded to unrestricted access: a broken containment root must make
    /// transcript consumers refuse the read.
    transcript_containment_root: Result<Option<PathBuf>, String>,
}

impl IncomingHookEvent {
    /// Build an ingress event, resolving `event`'s turn boundary through
    /// `driver`.
    ///
    /// This is the single point where a turn boundary enters the engine. The
    /// hook-callback ingress ([`handle_connection`]) calls it with the
    /// resolved hook-callback driver; a future stdout-JSONL reader (Codex,
    /// whose boundary is a native `turn.completed` event) calls it with its
    /// own driver and reaches the same consumers with no further plumbing.
    pub fn resolve(
        driver: &dyn AgentDriver,
        event: WorkerEvent,
        run_id: Option<String>,
        transcript_path: Option<String>,
        peer_pid: Option<libc::pid_t>,
    ) -> Self {
        let driver_resolution = DriverHookResolution {
            turn_boundary: driver.turn_boundary(&event),
            transcript_containment_root: match run_id.as_deref() {
                Some(run_id) => driver
                    .transcript_containment_root(run_id)
                    .map_err(|error| format!("{error:#}")),
                None => Ok(None),
            },
        };
        Self {
            peer_pid,
            run_id,
            transcript_path,
            event,
            driver_resolution,
        }
    }

    /// The driver-supplied turn-ended signal for this event, or `None` when
    /// the driver does not consider it a turn boundary.
    pub fn turn_boundary(&self) -> Option<&TurnEnd> {
        self.driver_resolution.turn_boundary.as_ref()
    }

    /// Whether this event ends a worker turn, per its driver. The gate every
    /// on-turn-boundary dispatcher opens with.
    pub fn is_turn_boundary(&self) -> bool {
        self.driver_resolution.turn_boundary.is_some()
    }

    pub fn transcript_containment_root(&self) -> Result<Option<&Path>, &str> {
        match &self.driver_resolution.transcript_containment_root {
            Ok(root) => Ok(root.as_deref()),
            Err(error) => Err(error),
        }
    }

    /// Test-only shorthand for [`Self::resolve`] against the engine's default
    /// hook-callback driver — the same one the production accept loop
    /// resolves via [`crate::driver::DriverRegistry`] — with no peer pid.
    /// Tests that need a *different* driver's boundary (or its absence)
    /// call `resolve` directly.
    #[cfg(test)]
    pub(crate) fn for_test(event: WorkerEvent, run_id: Option<String>, transcript_path: Option<String>) -> Self {
        let driver = crate::driver::DriverRegistry::default()
            .require(crate::effort::ENGINE_DEFAULT_DRIVER)
            .expect("engine default driver is always registered");
        Self::resolve(driver.as_ref(), event, run_id, transcript_path, None)
    }
}

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("events socket io: {0}")]
    Io(#[from] io::Error),
    #[error("hook payload was not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hook payload normalize: {0}")]
    Normalize(#[from] NormalizeError),
    /// The connection's driver could not be resolved deterministically —
    /// either the payload carried no `_boss_run_id`, the run id has no
    /// matching execution, or the resolved slug is not a registered driver.
    /// Never guessed around: the caller must log this and drop the
    /// connection rather than normalise against a fallback driver, since a
    /// silent fallback here is exactly the "every connection normalises as
    /// Claude" bug this per-connection resolution replaces.
    #[error("could not resolve driver for events-socket connection (run_id={run_id:?}): {reason}")]
    UnresolvedDriver { run_id: Option<String>, reason: String },
}

/// Bind+listen on the events socket at `path` and chmod the file to
/// 0600. This is synchronous — when this function returns Ok, the
/// socket is in the kernel's listening state, so a `connect()` from
/// another process will be queued in the accept backlog (not refused
/// with `ECONNREFUSED`) even before the caller polls `accept()` for
/// the first time. tokio's `UnixListener::bind` calls
/// `socket(2)` + `bind(2)` + `listen(2)` together; if `listen()`
/// fails the whole call returns the error, so there is no observable
/// "bound but not listening" intermediate state from the caller's
/// side.
///
/// Steps:
///   1. Ensure the parent directory exists.
///   2. Probe the path: if a live process is listening there, refuse —
///      see [`path_has_a_live_listener`]. Otherwise unlink it. A previous
///      engine that crashed without cleanup leaves a stale socket file
///      behind; if a fresh `bind()` ran without unlinking, on macOS
///      it would either return `EADDRINUSE` (if the kernel still
///      considers the inode bound) or — and this is the failure mode
///      the 2026-05-07 incident chased — the file would be replaced
///      but the new socket might never be put into the listen state
///      if some startup path reused the old fd. Just remove first.
///      `ENOENT` is the normal fresh-start case and is ignored;
///      every other error is fatal.
///   3. `UnixListener::bind` — atomic socket+bind+listen.
///   4. `chmod 0600` so only the boss-engine user can connect.
///
/// Errors are returned to the caller; the engine's `serve` propagates
/// them up to `main`, which records the failure in the audit log and
/// exits non-zero. A partially-bound socket never reaches the
/// dispatch loop.
pub fn bind_events_socket(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path_has_a_live_listener(path) {
        return Err(io::Error::other(format!(
            "refusing to unlink {}: a live process is already listening on it \
             (every resolution layer above this one either missed a collision or was bypassed)",
            path.display()
        )));
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::info!(
                events_socket_path = %path.display(),
                "events socket: unlinked stale file before bind",
            );
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let listener = UnixListener::bind(path)?;
    tracing::info!(
        events_socket_path = %path.display(),
        "events socket: bind+listen succeeded",
    );
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(listener)
}

/// Is a live process currently listening at `path`?
///
/// This is the last-line defence beneath the isolation guard's path
/// resolution (see `crate::app::isolation`): whatever route led here — a
/// correctly-derived fixture path, a resolution hole in the guard, or a bug
/// that bypasses it entirely — a socket file with nothing behind it is safe
/// to unlink and rebind, and one with a live process behind it never is. That
/// distinction is directly and cheaply testable without trusting anything
/// about how `path` was resolved.
///
/// Connecting and getting `Ok` proves a process called `listen()` on this
/// path and is still there to accept. `ECONNREFUSED` (socket file present,
/// nothing listening — a crashed engine's leftover) and `ENOENT` (nothing
/// there at all) both mean stale; any other error is treated as "can't tell,
/// assume stale" so a transient permission hiccup doesn't newly block
/// startup — the existing `remove_file` error handling right after this call
/// still surfaces a real problem.
pub(crate) fn path_has_a_live_listener(path: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_stream) => true,
        Err(_) => false,
    }
}

/// Look up the peer pid of a connected stream socket via
/// `getsockopt(SOL_LOCAL, LOCAL_PEERPID)` on macOS.
#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `fd` is borrowed from the caller's UnixStream and remains
    // valid for this call; `pid` and `len` are stack-local mutables and
    // their addresses are passed only to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            SOL_LOCAL,
            LOCAL_PEERPID,
            &mut pid as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Look up the peer pid of a connected stream socket via
/// `getsockopt(SO_PEERCRED)` on Linux.
#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is borrowed from the caller's UnixStream and remains
    // valid for this call; `cred` and `len` are stack-local mutables and
    // their addresses are passed only to `getsockopt`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.pid)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn peer_pid(_stream: &UnixStream) -> io::Result<libc::pid_t> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer pid lookup is not supported on this platform",
    ))
}

/// Read a connection to EOF and produce a typed IncomingHookEvent.
/// The shim half-closes its write side after writing the full hook
/// payload, so EOF is the message boundary.
///
/// Captures the peer pid synchronously before any await; if the
/// shim has already closed by then (its write is fast, then it
/// exits), the pid lookup may fail and the event is returned with
/// `peer_pid: None`.
///
/// `run_id` is extracted from the `_boss_run_id` field embedded in
/// the payload by the `boss-event` shim (sourced from `BOSS_RUN_ID` in
/// the worker's env). Every production event connection should carry
/// this field. If missing, a warning is logged but the event is
/// returned with `run_id: None`.
///
/// This socket is inherently a [`crate::driver::ProgressIngress::HookCallback`]
/// ingress — a `StdoutJsonl` driver never connects to it — but every
/// connection can carry a different worker's driver (Claude, Codex, Grok, …),
/// so the driver is resolved per connection rather than fixed once for the
/// whole accept loop. `registry` and `work_db` are injected rather than a
/// concrete driver: [`resolve_connection_driver`] uses `_boss_run_id` from the
/// decoded payload plus [`WorkDb::get_execution_driver_slug`] to look up the
/// run's actual driver slug, then resolves it through `registry` — the same
/// `tasks.driver` → `products.default_driver` → engine-default precedence
/// applied at spawn time.
pub async fn handle_connection(
    stream: UnixStream,
    registry: &DriverRegistry,
    work_db: &WorkDb,
) -> Result<IncomingHookEvent, SocketError> {
    let peer_pid_value = peer_pid(&stream).ok();
    let mut stream = stream;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let raw: serde_json::Value = serde_json::from_slice(&bytes)?;
    let payload_run_id = extract_run_id_from_payload(&raw);
    let run_id = if payload_run_id.is_none() {
        tracing::warn!("incoming hook event missing _boss_run_id field");
        None
    } else {
        payload_run_id
    };
    let driver = resolve_connection_driver(registry, work_db, run_id.as_deref(), peer_pid_value)?;
    // Decode through the driver's ProgressObservation and TranscriptAccess
    // capabilities. The decoded event drives the (driver-agnostic) activity
    // machine downstream, unchanged. `resolve` additionally asks the driver's
    // TurnBoundary capability whether this event ends a turn, so the
    // dispatchers never have to re-derive that from the event's shape.
    let transcript_path = driver.transcript_path_for_session(&raw);
    let event = driver.normalize_progress_event(&raw)?;
    Ok(IncomingHookEvent::resolve(
        driver.as_ref(),
        event,
        run_id,
        transcript_path,
        peer_pid_value,
    ))
}

/// Resolve the driver that governs `run_id`'s worker, deterministically.
///
/// This is the seam that replaces the old "every connection normalises as
/// the engine default" behaviour: a Grok run's hook events must be decoded by
/// [`crate::driver::GrokDriver`], not [`crate::driver::ClaudeDriver`], or they
/// are dropped with `MissingField` before they ever reach progress ingress.
///
/// Fails loudly — logging the connection's peer pid and run id — rather than
/// falling back to a default or retrying against every registered driver,
/// per the design constraint that a connection whose driver can't be
/// resolved must never be silently normalised against the wrong dialect.
fn resolve_connection_driver(
    registry: &DriverRegistry,
    work_db: &WorkDb,
    run_id: Option<&str>,
    peer_pid: Option<libc::pid_t>,
) -> Result<std::sync::Arc<dyn AgentDriver>, SocketError> {
    let Some(run_id) = run_id else {
        tracing::error!(
            ?peer_pid,
            "events socket: cannot resolve a driver for this connection — payload carried no _boss_run_id",
        );
        return Err(SocketError::UnresolvedDriver {
            run_id: None,
            reason: "connection payload had no _boss_run_id".to_owned(),
        });
    };
    let slug = work_db.get_execution_driver_slug(run_id).map_err(|error| {
        tracing::error!(
            run_id,
            ?peer_pid,
            %error,
            "events socket: driver-slug lookup failed for this connection's run_id",
        );
        SocketError::UnresolvedDriver {
            run_id: Some(run_id.to_owned()),
            reason: format!("driver-slug lookup failed: {error:#}"),
        }
    })?;
    let Some(slug) = slug else {
        tracing::error!(
            run_id,
            ?peer_pid,
            "events socket: no execution found for this connection's run_id — cannot resolve driver",
        );
        return Err(SocketError::UnresolvedDriver {
            run_id: Some(run_id.to_owned()),
            reason: "no execution/task row found for run_id".to_owned(),
        });
    };
    registry.require(&slug).map_err(|error| {
        tracing::error!(
            run_id,
            ?peer_pid,
            driver_slug = %slug,
            "events socket: resolved driver slug is not registered in this binary",
        );
        SocketError::UnresolvedDriver {
            run_id: Some(run_id.to_owned()),
            reason: format!("{error}"),
        }
    })
}

/// Publish the two event-bus transitions this hook-ingress path can
/// observe directly, per the event-bus design doc's taxonomy
/// (`tools/boss/docs/designs/engine-event-bus-event-driven-reconcilers-via-an-in-process-message-queue.md`).
/// No subscribers are wired up yet — this only stages the publishers so a
/// later PR can land `stranded_answering_sweep` / `transient_recovery` as
/// thin subscribers without touching this ingress path again.
///
/// Events are hints, not commands: a subscriber always re-reads
/// authoritative DB state before acting (e.g. whether this run's
/// execution is actually a live `answer_agent` with a pending question),
/// so publishing on a run this ingress can't fully classify is harmless
/// — the eventual subscriber's own check no-ops on a false positive.
///
/// - [`Event::TransientErrorIdle`]: published on the driver's turn
///   boundary when the transcript's last meaningful entry is a trailing
///   API error — the same ground truth [`crate::transient_recovery`]'s
///   sweep already trusts, just checked immediately instead of waiting
///   for the next 60s pass.
/// - [`Event::AnswerAgentDied`]: published on `SessionEnd`, the only
///   hook that fires when a worker's session terminates. This does not
///   cover a hard pane kill (no hook fires at all in that case) — that
///   case has no in-process event and stays reliant on
///   `stranded_answering_sweep`'s DB-driven backstop, unchanged.
///   `SessionEnd` is a process boundary, not a turn boundary, so it stays
///   an event-shape match.
pub async fn publish_hook_derived_events(bus: &EventBus, incoming: &IncomingHookEvent) {
    let Some(execution_id) = incoming.run_id.clone() else {
        return;
    };
    if incoming.is_turn_boundary() {
        let Some(transcript_path) = incoming.transcript_path.as_deref() else {
            return;
        };
        let lines = crate::transient_recovery::read_transcript_tail(
            transcript_path,
            crate::transient_recovery::TRANSCRIPT_TAIL_MAX_BYTES,
        )
        .await;
        if crate::transient_error::extract_worker_error(&lines).is_some() {
            bus.publish(Event::TransientErrorIdle { execution_id });
        }
        return;
    }
    if let WorkerEvent::SessionEnd { .. } = &incoming.event {
        bus.publish(Event::AnswerAgentDied { execution_id });
    }
}

/// Pull `_boss_run_id` out of the raw hook payload if the shim
/// embedded it. Empty strings are treated as missing so a stray
/// `BOSS_RUN_ID=` doesn't poison correlation with an empty id.
fn extract_run_id_from_payload(raw: &serde_json::Value) -> Option<String> {
    let s = raw.get("_boss_run_id")?.as_str()?;
    if s.is_empty() { None } else { Some(s.to_owned()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::WorkItemPatch;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    /// Build a `WorkDb` with one ready chore execution whose resolved driver
    /// slug is `driver_slug` (via `products.default_driver`), or the engine
    /// default when `None`. Returns the DB's owning `TempDir` (kept alive for
    /// the caller's scope) alongside the `WorkDb` and the execution id to
    /// embed as `_boss_run_id` in a test payload — mirroring how
    /// `resolve_connection_driver` actually looks a connection's driver up in
    /// production.
    fn db_with_ready_execution(driver_slug: Option<&str>) -> (TempDir, WorkDb, String) {
        let (dir, db) = crate::test_support::open_db();
        let product = crate::test_support::create_test_product(&db);
        if let Some(slug) = driver_slug {
            db.update_work_item(
                &product.id,
                WorkItemPatch {
                    default_driver: Some(slug.to_owned()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let chore = crate::test_support::create_test_chore(&db, &product.id, "events-socket test chore");
        let execution = crate::test_support::create_ready_chore_execution(&db, &chore.id);
        (dir, db, execution.id)
    }

    /// A socket filename that is unique across every test invocation in this
    /// process, not just across the (already-unique) `TempDir` it lives in.
    /// Belt-and-suspenders for the two tests below that rebind at the same
    /// path twice within one test: extra entropy costs nothing and rules out
    /// any path-level ambiguity beyond what `TempDir` alone provides.
    fn unique_socket_path(dir: &TempDir) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.path().join(format!(
            "events-{}-{:?}-{n}.sock",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    /// Poll `path_has_a_live_listener` until it reports false. Dropping a
    /// `UnixListener` closes its fd synchronously, but that only guarantees
    /// *this process* sees the fd gone — it makes no promise about how
    /// quickly the kernel finishes tearing down the listening socket's
    /// internal state such that a fresh `connect()` reliably observes it as
    /// gone too. A production restart never exercises that narrow window
    /// (the old process is long dead before a new one binds); this test's
    /// drop-then-immediately-rebind in the same process is what makes the
    /// window observable. Waiting here tests the real invariant — the old
    /// listener eventually goes away — without asserting a zero-latency
    /// guarantee the kernel doesn't make.
    async fn wait_for_teardown(path: &Path) {
        for _ in 0..200 {
            if !path_has_a_live_listener(path) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("listener at {} did not tear down within 2s", path.display());
    }

    #[tokio::test]
    async fn bind_creates_socket_with_mode_0600() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();

        assert!(path.exists());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[tokio::test]
    async fn bind_replaces_stale_socket_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        std::fs::write(&path, b"stale").unwrap();
        let _listener = bind_events_socket(&path).unwrap();
        assert!(path.exists());
        // The file is now a socket, not a regular file with "stale" content.
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
    }

    #[tokio::test]
    async fn bind_creates_parent_directory() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");
        let path = nested.join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();
        assert!(path.exists());
    }

    /// Regression test for the 2026-05-07 incident: after
    /// `bind_events_socket` returns, the kernel must already be in the
    /// listen state. A `connect()` from a separate thread must
    /// succeed (not return ECONNREFUSED) even before the caller polls
    /// `accept()`. tokio's `UnixListener::bind` covers this — this
    /// test pins the contract so a refactor that splits bind from
    /// listen across async hops fails loudly.
    #[tokio::test]
    async fn connect_succeeds_immediately_after_bind() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let _listener = bind_events_socket(&path).unwrap();
        // No `accept()` yet — the connect must be queued in the
        // backlog by the kernel, not refused.
        let path_for_thread = path.clone();
        let connected = std::thread::spawn(move || StdUnixStream::connect(&path_for_thread))
            .join()
            .unwrap();
        assert!(
            connected.is_ok(),
            "connect() right after bind must succeed, got {:?}",
            connected.err()
        );
    }

    /// A previous engine that crashed without cleanup leaves a
    /// dangling socket file. The new engine must unlink it cleanly
    /// and the rebound socket must be in the listen state. (The
    /// `bind_replaces_stale_socket_file` test above checks the file
    /// type swap; this one checks the listen-state of the rebound
    /// socket — the bug's load-bearing assertion.)
    #[tokio::test]
    async fn rebind_after_stale_file_listens() {
        let dir = TempDir::new().unwrap();
        let path = unique_socket_path(&dir);

        // Round 1: bind, then drop the listener. The on-disk socket
        // file persists (close(2) doesn't unlink AF_UNIX paths).
        {
            let _listener = bind_events_socket(&path).unwrap();
        }
        assert!(path.exists(), "stale socket file should remain after drop");
        // Give the kernel a moment to finish tearing down the dropped
        // listener's socket state before probing it again — see
        // `wait_for_teardown`'s doc comment.
        wait_for_teardown(&path).await;

        // Round 2: rebind. Must unlink + listen successfully.
        let _listener = bind_events_socket(&path).unwrap();
        let path_for_thread = path.clone();
        let connected = std::thread::spawn(move || StdUnixStream::connect(&path_for_thread))
            .join()
            .unwrap();
        assert!(
            connected.is_ok(),
            "connect() after rebind must succeed, got {:?}",
            connected.err()
        );
    }

    /// The counterpart to `rebind_after_stale_file_listens`: a socket file
    /// with a live process behind it must never be unlinked; only a crashed
    /// engine's leftover is safe to rebind.
    #[tokio::test]
    async fn refuses_to_steal_a_live_listener() {
        let dir = TempDir::new().unwrap();
        let path = unique_socket_path(&dir);

        // Round 1: a real listener stays alive on `path` for the whole test.
        let _live_listener = bind_events_socket(&path).unwrap();

        // Round 2: a second bind attempt at the same path must be refused,
        // and the first listener must still be reachable afterward.
        let err = bind_events_socket(&path).expect_err("must not steal a live listener's socket");
        assert!(
            format!("{err}").contains("live process"),
            "error should name the reason; got: {err}"
        );

        let path_for_thread = path.clone();
        let connected = std::thread::spawn(move || StdUnixStream::connect(&path_for_thread))
            .join()
            .unwrap();
        assert!(
            connected.is_ok(),
            "the original live listener must still be reachable after the refused steal attempt"
        );
    }

    #[tokio::test]
    async fn round_trip_hook_payload_through_socket() {
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // Mimic the shim: connect, write, then close. The peer_pid
        // lookup is best-effort under this race — it might be Some or
        // None depending on scheduling. We assert only on the event
        // payload here; the explicit pid-matching test below holds the
        // client alive for the duration of the lookup.
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"sess-1","stop_hook_active":false,"_boss_run_id":"{run_id}"}}"#
        );
        let path_owned = path.clone();
        let client_task = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        client_task.await.unwrap();

        match incoming.event {
            WorkerEvent::Stop { session_id, .. } => assert_eq!(session_id, "sess-1"),
            other => panic!("expected Stop, got {other:?}"),
        }
        assert_eq!(incoming.run_id.as_deref(), Some(run_id.as_str()));
    }

    #[tokio::test]
    async fn transcript_path_extracted_from_payload() {
        // Claude stamps `transcript_path` on every hook payload. We
        // surface it here so the engine can persist it on the
        // `work_runs` row — without this round-trip, the live-status
        // summarizer's tail watcher has no file to open and the per-slot
        // loop early-outs every tick on "no transcript path yet".
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"transcript_path":"/home/u/.claude/projects/foo/sess-1.jsonl","_boss_run_id":"{run_id}"}}"#
        );
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        client.await.unwrap();

        assert_eq!(
            incoming.transcript_path.as_deref(),
            Some("/home/u/.claude/projects/foo/sess-1.jsonl"),
        );
    }

    #[tokio::test]
    async fn missing_transcript_path_is_none() {
        // Pre-live-status hook payloads (and the test fixtures still
        // around) won't carry the field. The extractor must surface
        // `None` rather than erroring or stalling the dispatcher.
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"{run_id}"}}"#
        );
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        client.await.unwrap();

        assert!(incoming.transcript_path.is_none());
    }

    #[tokio::test]
    async fn empty_transcript_path_is_none() {
        // An empty string would round-trip through SQLite into a
        // path the tail watcher would try (and fail) to open every
        // tick. Treat empty as missing, matching the `_boss_run_id`
        // policy.
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"transcript_path":"","_boss_run_id":"{run_id}"}}"#
        );
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        client.await.unwrap();

        assert!(incoming.transcript_path.is_none());
    }

    #[tokio::test]
    async fn run_id_extracted_from_payload_field() {
        // The `_boss_run_id` field embedded by the shim is the only path by
        // which the engine resolves a run id (and, per this seam, the
        // connection's driver) today — it must name a real execution or
        // resolution fails loudly (see `unresolved_run_id_fails_loudly`).
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"{run_id}"}}"#
        );
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        client.await.unwrap();

        assert_eq!(incoming.run_id.as_deref(), Some(run_id.as_str()));
    }

    #[tokio::test]
    async fn payload_run_id_wins_over_missing_fallback() {
        // The `_boss_run_id` field in the payload is the only path for
        // run correlation. This test confirms it's correctly extracted.
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let payload = format!(
            r#"{{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"{run_id}"}}"#
        );
        let (close_tx, close_rx) = std::sync::mpsc::channel::<()>();
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let _ = close_rx.recv();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db).await.unwrap();
        assert_eq!(incoming.run_id.as_deref(), Some(run_id.as_str()));

        close_tx.send(()).ok();
        client.join().unwrap();
    }

    #[tokio::test]
    async fn malformed_json_yields_socket_error() {
        let (_db_dir, db, _run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(b"not json").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream, &registry, &db).await;
        client.await.unwrap();

        match result {
            Err(SocketError::Json(_)) => {}
            other => panic!("expected Json error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn known_event_with_unknown_kind_yields_normalize_error() {
        let (_db_dir, db, run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let payload = format!(r#"{{"session_id":"x","hook_event_name":"WeirdHook","_boss_run_id":"{run_id}"}}"#);
        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream, &registry, &db).await;
        client.await.unwrap();

        match result {
            Err(SocketError::Normalize(NormalizeError::UnknownEvent(name))) => {
                assert_eq!(name, "WeirdHook");
            }
            other => panic!("expected Normalize/UnknownEvent, got {other:?}"),
        }
    }

    // ─── per-connection driver resolution ──────────────────────────────

    /// Minimal driver that decodes Grok's camelCase hook-payload shape
    /// (`hookEventName` / `sessionId`) instead of Claude's snake_case
    /// (`hook_event_name` / `session_id`). Stands in for the real
    /// `GrokDriver::normalize_progress_event` — which is `unimplemented!()`
    /// pending the separate T-10 dialect work — so this suite can exercise
    /// per-connection driver *resolution* (this bug) independently of
    /// Grok's actual dialect decoding (that follow-on).
    struct CamelCaseTestDriver;

    #[async_trait::async_trait]
    impl AgentDriver for CamelCaseTestDriver {
        fn descriptor(&self) -> &crate::driver::DriverDescriptor {
            static DESCRIPTOR: std::sync::OnceLock<crate::driver::DriverDescriptor> = std::sync::OnceLock::new();
            DESCRIPTOR.get_or_init(crate::driver::test_support::stub_descriptor)
        }
        fn capabilities(&self) -> crate::driver::CapabilitySet {
            crate::driver::CapabilitySet::new([])
        }
        fn spawn_invocation(&self, _request: crate::driver::SpawnRequest<'_>) -> crate::driver::SpawnPlan {
            unimplemented!()
        }
        async fn provision_workspace(
            &self,
            _workspace: &Path,
            _prompt_text: &str,
            _run_id: &str,
        ) -> anyhow::Result<Option<crate::driver::DriverRuntimeState>> {
            unimplemented!()
        }
        async fn teardown_workspace(
            &self,
            _workspace: Option<&Path>,
            _run_id: &str,
            _runtime_state: Option<&crate::driver::DriverRuntimeState>,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn write_permission_config(
            &self,
            _input: &crate::driver::PermissionInput,
            _dest_dir: &Path,
        ) -> anyhow::Result<crate::driver::PermissionArtifacts> {
            unimplemented!()
        }
        fn progress_fidelity(&self) -> crate::driver::ProgressFidelity {
            crate::driver::ProgressFidelity::Minimal
        }
        fn progress_observation_wiring(
            &self,
            _config: &crate::driver::ProgressObservationConfig,
        ) -> crate::driver::ProgressIngress {
            crate::driver::ProgressIngress::StdoutJsonl
        }
        fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
            let obj = raw
                .as_object()
                .ok_or_else(|| NormalizeError::Malformed("expected JSON object".to_owned()))?;
            let hook_event_name = obj
                .get("hookEventName")
                .and_then(|v| v.as_str())
                .ok_or(NormalizeError::MissingField("hookEventName"))?;
            let session_id = obj
                .get("sessionId")
                .and_then(|v| v.as_str())
                .ok_or(NormalizeError::MissingField("sessionId"))?
                .to_owned();
            match hook_event_name {
                "Stop" => Ok(WorkerEvent::Stop {
                    session_id,
                    stop_hook_active: false,
                    stop_reason: boss_protocol::StopReason::Completed,
                }),
                other => Err(NormalizeError::UnknownEvent(other.to_owned())),
            }
        }
        fn turn_boundary(&self, _event: &WorkerEvent) -> Option<TurnEnd> {
            None
        }
        fn tool_use_interception_wiring(
            &self,
            _config: &crate::driver::ToolUseInterceptionConfig,
        ) -> crate::driver::ToolUseInterceptionWiring {
            crate::driver::ToolUseInterceptionWiring {
                pre_tool_use_hooks: Vec::new(),
            }
        }
        fn agent_rules_preamble(&self) -> &'static str {
            "# camelcase test driver preamble\n"
        }
        fn transcript_path_for_session(&self, _raw: &serde_json::Value) -> Option<String> {
            None
        }
        fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
            raw
        }
        fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
            None
        }
        fn classify_error(&self, _raw_output: &str) -> crate::driver::WorkerErrorClass {
            unimplemented!()
        }
        fn structured_output_fallback(
            &self,
            _kind: boss_engine_structured_output::StructuredOutputKind,
            _text: &str,
        ) -> Vec<boss_engine_structured_output::fallback::FallbackCandidate> {
            Vec::new()
        }
    }

    /// Regression for the bug this seam fixes: a Grok-dialect (camelCase)
    /// hook payload arriving on the events socket for a run whose execution
    /// driver is `grok` must be normalised by the driver registered for
    /// `grok` — not silently normalised (and dropped) as Claude. Before this
    /// change every connection resolved `ENGINE_DEFAULT_DRIVER` regardless of
    /// the run, so this payload would have failed
    /// `ClaudeDriver::normalize_progress_event` with
    /// `MissingField("session_id")` and never reached progress ingress.
    #[tokio::test]
    async fn grok_dialect_payload_is_normalised_by_the_runs_grok_driver_not_the_engine_default() {
        let (_db_dir, db, run_id) = db_with_ready_execution(Some("grok"));
        // `GrokDriver::normalize_progress_event` is `unimplemented!()` pending
        // T-10; substitute a driver with a real camelCase decoder under the
        // "grok" slug so this test proves *resolution*, not T-10's dialect.
        let registry = DriverRegistry::default().with_driver("grok", std::sync::Arc::new(CamelCaseTestDriver));
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let payload = format!(r#"{{"hookEventName":"Stop","sessionId":"grok-sess-1","_boss_run_id":"{run_id}"}}"#);
        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let incoming = handle_connection(stream, &registry, &db)
            .await
            .expect("camelCase payload must be normalised by the resolved grok driver, not dropped");
        client.await.unwrap();

        match incoming.event {
            WorkerEvent::Stop { session_id, .. } => assert_eq!(session_id, "grok-sess-1"),
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    /// Counterpart to the test above: the identical camelCase payload,
    /// arriving on a run whose execution driver is `claude`, must still be
    /// rejected by `ClaudeDriver::normalize_progress_event` exactly as it is
    /// today — per-connection resolution must not change Claude's behaviour.
    #[tokio::test]
    async fn same_camelcase_payload_for_a_claude_run_still_fails_normalize_as_today() {
        let (_db_dir, db, run_id) = db_with_ready_execution(Some("claude"));
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let payload = format!(r#"{{"hookEventName":"Stop","sessionId":"grok-sess-1","_boss_run_id":"{run_id}"}}"#);
        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream, &registry, &db).await;
        client.await.unwrap();

        match result {
            Err(SocketError::Normalize(NormalizeError::MissingField(field))) => {
                assert_eq!(field, "session_id");
            }
            other => panic!("expected Normalize/MissingField(\"session_id\"), got {other:?}"),
        }
    }

    /// A connection whose payload carries no `_boss_run_id` at all must fail
    /// loudly rather than silently falling back to a guessed driver.
    #[tokio::test]
    async fn missing_run_id_fails_loudly_instead_of_guessing_a_driver() {
        let (_db_dir, db, _run_id) = db_with_ready_execution(None);
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream
                .write_all(br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false}"#)
                .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream, &registry, &db).await;
        client.await.unwrap();

        match result {
            Err(SocketError::UnresolvedDriver { run_id: None, .. }) => {}
            other => panic!("expected UnresolvedDriver{{run_id: None}}, got {other:?}"),
        }
    }

    /// A `_boss_run_id` that names no known execution must also fail loudly
    /// — never fall back to guessing a driver for an unrecognised run.
    #[tokio::test]
    async fn unknown_run_id_fails_loudly_instead_of_guessing_a_driver() {
        let (_db_dir, db) = crate::test_support::open_db();
        let registry = DriverRegistry::default();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        let path_owned = path.clone();
        let client = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(
                br#"{"hook_event_name":"Stop","session_id":"s","stop_hook_active":false,"_boss_run_id":"no-such-execution"}"#,
            ).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = handle_connection(stream, &registry, &db).await;
        client.await.unwrap();

        match result {
            Err(SocketError::UnresolvedDriver { run_id: Some(id), .. }) => {
                assert_eq!(id, "no-such-execution");
            }
            other => panic!("expected UnresolvedDriver{{run_id: Some(..)}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn peer_pid_matches_self_when_client_stays_connected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("events.sock");
        let listener = bind_events_socket(&path).unwrap();

        // Hold the client open until the server has captured peer_pid.
        let (close_tx, close_rx) = std::sync::mpsc::channel::<()>();
        let path_owned = path.clone();
        let payload = b"{}";
        let client = std::thread::spawn(move || {
            use std::io::Write;
            let mut stream = StdUnixStream::connect(&path_owned).unwrap();
            stream.write_all(payload).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            // Block in the thread, keeping the stream alive (its
            // descriptor is still owned by `stream`) until the server
            // signals we can drop it.
            let _ = close_rx.recv();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let observed_pid = peer_pid(&stream).unwrap();
        let self_pid = std::process::id() as libc::pid_t;
        assert_eq!(observed_pid, self_pid);

        // Release the client.
        close_tx.send(()).ok();
        client.join().unwrap();
    }

    // ─── publish_hook_derived_events ───────────────────────────────────

    fn incoming_stop(run_id: &str, transcript_path: Option<&str>) -> IncomingHookEvent {
        IncomingHookEvent::for_test(
            WorkerEvent::Stop {
                session_id: "s".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
            Some(run_id.to_owned()),
            transcript_path.map(str::to_owned),
        )
    }

    fn write_transcript(dir: &TempDir, name: &str, lines: &[&str]) -> String {
        use std::io::Write;
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    const NORMAL_LINE: &str =
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#;
    const API_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}}"#;

    #[tokio::test]
    async fn stop_with_trailing_api_error_publishes_transient_error_idle() {
        let dir = TempDir::new().unwrap();
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, API_ERROR_LINE]);
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());

        publish_hook_derived_events(&bus, &incoming_stop("exec-1", Some(&transcript))).await;

        let event = sub.recv().await.expect("event published");
        assert_eq!(
            event,
            Event::TransientErrorIdle {
                execution_id: "exec-1".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn stop_with_no_trailing_error_publishes_nothing() {
        let dir = TempDir::new().unwrap();
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE]);
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());

        publish_hook_derived_events(&bus, &incoming_stop("exec-1", Some(&transcript))).await;

        // Publish an unrelated event so the assertion below can distinguish
        // "nothing published" from "recv would hang forever".
        bus.publish(Event::DispatchReady);
        let event = sub.recv().await.expect("the sentinel event should still arrive");
        assert_eq!(
            event,
            Event::DispatchReady,
            "no TransientErrorIdle should have been published"
        );
    }

    #[tokio::test]
    async fn stop_with_no_transcript_path_publishes_nothing() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());

        publish_hook_derived_events(&bus, &incoming_stop("exec-1", None)).await;

        bus.publish(Event::DispatchReady);
        let event = sub.recv().await.expect("the sentinel event should still arrive");
        assert_eq!(event, Event::DispatchReady);
    }

    #[tokio::test]
    async fn session_end_publishes_answer_agent_died() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());
        let incoming = IncomingHookEvent::for_test(
            WorkerEvent::SessionEnd {
                session_id: "s".to_owned(),
                reason: "exit".to_owned(),
            },
            Some("exec-2".to_owned()),
            None,
        );

        publish_hook_derived_events(&bus, &incoming).await;

        let event = sub.recv().await.expect("event published");
        assert_eq!(
            event,
            Event::AnswerAgentDied {
                execution_id: "exec-2".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn missing_run_id_publishes_nothing() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());
        let incoming = IncomingHookEvent::for_test(
            WorkerEvent::SessionEnd {
                session_id: "s".to_owned(),
                reason: "exit".to_owned(),
            },
            None,
            None,
        );

        publish_hook_derived_events(&bus, &incoming).await;

        bus.publish(Event::DispatchReady);
        let event = sub.recv().await.expect("the sentinel event should still arrive");
        assert_eq!(event, Event::DispatchReady);
    }

    // ─── turn boundary (Capability::TurnBoundary) ──────────────────────

    #[tokio::test]
    async fn ingress_resolves_the_turn_boundary_through_the_driver() {
        // The whole point of the seam: the boundary on the ingress event is
        // whatever the driver said, carried alongside the decoded event.
        let stop = incoming_stop("exec-1", None);
        let end = stop.turn_boundary().expect("Claude's Stop is a turn boundary");
        assert_eq!(end.session_id, "s");
        assert!(!end.continuation);
        assert!(stop.is_turn_boundary());
    }

    #[tokio::test]
    async fn ingress_reports_no_boundary_for_a_mid_turn_event() {
        let incoming = IncomingHookEvent::for_test(
            WorkerEvent::PostToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
                tool_response: serde_json::json!({}),
            },
            Some("exec-1".to_owned()),
            None,
        );
        assert!(!incoming.is_turn_boundary());
        assert!(incoming.turn_boundary().is_none());
    }

    #[tokio::test]
    async fn a_driver_that_declares_no_boundary_suppresses_the_transient_error_publish() {
        // A `Stop`-shaped event from a driver that does not call it a turn
        // boundary must not open the on-turn-boundary path. This is the
        // behaviour a hardcoded `WorkerEvent::Stop` match could not express.
        let dir = TempDir::new().unwrap();
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, API_ERROR_LINE]);
        let bus = EventBus::new();
        let mut sub = bus.subscribe(boss_event_bus::TopicFilter::all());

        let boundaryless = crate::driver::test_support::StubDriver::new(
            crate::driver::test_support::stub_descriptor(),
            crate::driver::CapabilitySet::new([]),
        );
        let incoming = IncomingHookEvent::resolve(
            &boundaryless,
            WorkerEvent::Stop {
                session_id: "s".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
            Some("exec-1".to_owned()),
            Some(transcript),
            None,
        );
        assert!(!incoming.is_turn_boundary());

        publish_hook_derived_events(&bus, &incoming).await;

        bus.publish(Event::DispatchReady);
        let event = sub.recv().await.expect("the sentinel event should still arrive");
        assert_eq!(event, Event::DispatchReady);
    }
}
