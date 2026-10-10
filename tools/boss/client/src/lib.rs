//! Reusable client for talking to the Boss engine over its frontend socket.
//!
//! `BossClient` opens a Unix-domain connection to the engine and provides a
//! correlated request/response API on top of the framed JSON protocol defined
//! in [`boss_protocol`]. Engine discovery (socket path resolution + optional
//! autostart of the engine binary) lives behind [`Discovery`] so the CLI, tests,
//! and future TUI/web frontends share one set of rules.

mod replay;
mod retry;
#[cfg(test)]
mod retry_tests;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use boss_protocol::{FrontendEvent, FrontendEventEnvelope, FrontendRequest, FrontendRequestEnvelope};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{sleep, timeout};

pub use replay::{DUPLICATE_GUARD_WINDOW, ReplaySafety, replay_safety, replay_safety_with, request_name};
pub use retry::{DEFAULT_MAX_WAIT, EngineUnreachable, MAX_WAIT_ENV, NoticeSink, OutcomeUnknown, RetryPolicy};
use retry::{RetryState, is_unreachable_kind};

pub const LEGACY_SOCKET_PATH: &str = "/tmp/boss-engine.sock";
pub const LEGACY_PID_PATH: &str = "/tmp/boss-engine.pid";
pub const DEFAULT_ENGINE_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound on one connect attempt. A Unix-socket connect either succeeds
/// or fails promptly unless the listener's backlog is wedged; this keeps a
/// wedged engine from hanging a single attempt past the retry budget.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// Floor for a connect attempt whose budget is nearly spent.
const MIN_CONNECT_TIMEOUT: Duration = Duration::from_millis(50);
/// Set in every Boss worker session (by the engine, at spawn).
const WORKER_RUN_ENV: &str = "BOSS_RUN_ID";

#[derive(Debug, Clone)]
pub struct EngineCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Short label describing how `program` was resolved. Surfaced in
    /// error messages so a failed autostart names the source it picked
    /// (e.g. `BOSS_ENGINE_BIN`, a `bazel-bin` lookup,
    /// `PATH lookup`).
    pub source: String,
    /// Ordered list of every resolution step the resolver tried. The
    /// last entry is the one that won. Included verbatim in autostart
    /// error messages so the next person can debug a misconfigured
    /// resolution chain.
    pub attempted: Vec<String>,
}

/// Inputs to [`resolve_engine_command_with`] — split out so tests can
/// drive the resolver deterministically without mutating process env.
#[derive(Debug, Clone, Default)]
pub struct EngineResolverInput {
    pub env_cmd: Option<String>,
    pub env_bin: Option<String>,
    pub workspace_root: Option<PathBuf>,
    pub current_exe: Option<PathBuf>,
}

/// How a client should locate the engine and (optionally) launch it.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub socket_path: String,
    pub pid_file_path: String,
    pub legacy_socket_path: Option<String>,
    pub legacy_pid_file_path: Option<String>,
    pub control_token_path: PathBuf,
    pub autostart: bool,
    pub engine: EngineCommand,
    pub launch_directory: PathBuf,
    pub start_timeout: Duration,
    /// How long, and how patiently, to wait for an unreachable engine.
    pub retry: RetryPolicy,
    /// This process is a Boss worker session. Workers must never start an
    /// engine, so [`Self::with_autostart`] cannot turn autostart on for one.
    pub worker_environment: bool,
    /// How far back the engine's duplicate-create guard looks. A guarded
    /// create is only resent while this window (minus a margin) is open; see
    /// [`replay_safety_with`]. Injectable so tests need not wait a real minute.
    pub duplicate_guard_window: Duration,
}

impl Discovery {
    /// Build a discovery profile from process env + an optional `--socket-path` override.
    pub fn from_env(socket_override: Option<&str>) -> Result<Self> {
        let explicit_socket = socket_override
            .map(str::to_owned)
            .or_else(|| std::env::var("BOSS_SOCKET_PATH").ok());
        let (socket_path, legacy_socket_path) = match explicit_socket.as_deref() {
            Some(path) => (path.to_owned(), None),
            None => (
                boss_log_files::default_frontend_socket_path()
                    .context("HOME must be set to derive the default engine socket path")?
                    .to_string_lossy()
                    .into_owned(),
                Some(LEGACY_SOCKET_PATH.to_owned()),
            ),
        };
        let explicit_pid = std::env::var("BOSS_ENGINE_PID_PATH").ok();
        let pid_file_path = match explicit_pid {
            Some(path) => path,
            None if explicit_socket.is_some() => derived_sibling_path(&socket_path, "pid"),
            None => boss_log_files::default_engine_pid_path()
                .context("HOME must be set to derive the default engine pid-file path")?
                .to_string_lossy()
                .into_owned(),
        };
        let legacy_pid_file_path = explicit_socket.is_none().then(|| LEGACY_PID_PATH.to_owned());
        let control_token_path = match std::env::var_os("BOSS_ENGINE_CONTROL_TOKEN_PATH") {
            Some(path) => PathBuf::from(path),
            None if explicit_socket.is_some() => PathBuf::from(derived_sibling_path(&socket_path, "control-token")),
            None => default_control_token_path()
                .context("HOME must be set to derive the default engine-control token path")?,
        };
        let launch_directory = resolve_launch_directory()?;
        let engine = resolve_engine_command(&socket_path)?;
        let worker_environment = non_empty_env(WORKER_RUN_ENV).is_some();

        Ok(Self {
            socket_path,
            pid_file_path,
            legacy_socket_path,
            legacy_pid_file_path,
            control_token_path,
            autostart: !worker_environment,
            engine,
            launch_directory,
            start_timeout: DEFAULT_ENGINE_START_TIMEOUT,
            retry: RetryPolicy::from_env(),
            worker_environment,
            duplicate_guard_window: DUPLICATE_GUARD_WINDOW,
        })
    }

    /// Allow or forbid transparently starting the engine. Ignored (stays
    /// off) in a worker environment.
    pub fn with_autostart(mut self, autostart: bool) -> Self {
        self.autostart = autostart && !self.worker_environment;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Apply the CLI's explicit retry opt-outs on top of the env-derived
    /// policy: `no_retry` fails on the first unreachable attempt, otherwise
    /// `max_wait_secs` (if given) replaces the budget.
    pub fn with_retry_overrides(mut self, no_retry: bool, max_wait_secs: Option<u64>) -> Self {
        if no_retry {
            self.retry.max_wait = Duration::ZERO;
        } else if let Some(secs) = max_wait_secs {
            self.retry.max_wait = Duration::from_secs(secs);
        }
        self
    }

    fn endpoint_candidates(&self) -> Vec<(&str, &str)> {
        let mut endpoints = vec![(self.socket_path.as_str(), self.pid_file_path.as_str())];
        if let (Some(socket), Some(pid)) = (&self.legacy_socket_path, &self.legacy_pid_file_path) {
            endpoints.push((socket.as_str(), pid.as_str()));
        }
        endpoints
    }
}

fn derived_sibling_path(socket_path: &str, suffix: &str) -> String {
    let path = Path::new(socket_path);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|value| value.to_str()).unwrap_or("boss-test");
    directory
        .join(format!("{stem}.{suffix}"))
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningEngine {
    pub socket_path: String,
    pub pid_file_path: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStopOutcome {
    Stopped,
    AlreadyStopped,
}

/// Single-connection client over the engine's frontend socket.
///
/// A client made by [`BossClient::connect`] survives a brief engine outage:
/// the connect itself backs off until the engine appears, and
/// [`send_request`](Self::send_request) reconnects and resends when that is
/// provably safe (see [`replay_safety`]). One made by
/// [`connect_socket`](Self::connect_socket) is a bare single connection with
/// no recovery, for callers (engine control, tests) that want exact control.
#[derive(Debug)]
pub struct BossClient {
    reader: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_request_id: AtomicU64,
    /// Endpoint this connection is on, for error messages.
    socket_path: String,
    /// How to get a fresh connection; `None` disables recovery.
    reconnect: Option<Discovery>,
}

/// Why one send/receive exchange failed, split by what that proves about
/// whether the engine saw the request.
enum Exchange {
    /// The request was provably not delivered: the very first write failed,
    /// so zero bytes reached the socket. Anything short of that proof is
    /// [`Exchange::Dropped`] — a partial write may still leave a complete
    /// JSON line the engine parses at EOF, and a flush error can follow a
    /// fully written line.
    NotSent(anyhow::Error),
    /// The request may have been delivered but no response arrived (EOF or
    /// read error, a partial write, or a flush error): the engine may have
    /// applied it before it went away.
    Dropped(anyhow::Error),
    /// Not a transport problem (e.g. an undecodable reply); never retried.
    Fatal(anyhow::Error),
}

impl BossClient {
    /// Connect to the engine, optionally autostarting it per the discovery
    /// profile, waiting out a briefly unavailable engine per
    /// [`Discovery::retry`].
    ///
    /// Unreachable means: socket file missing, connection refused, connect
    /// timeout, or the engine closing the connection. Those are retried with
    /// exponential backoff and jitter until the budget is spent, then fail
    /// with [`EngineUnreachable`] naming the socket and the time waited.
    /// With autostart on, the engine is started at most once per call; if one
    /// is already starting or restarting (live pid file) it is waited for
    /// rather than racing a second. Autostart is always off in a worker.
    pub async fn connect(discovery: &Discovery) -> Result<Self> {
        let mut retry = RetryState::new(&discovery.retry);
        Self::connect_within(discovery, &mut retry).await
    }

    async fn connect_within(discovery: &Discovery, retry: &mut RetryState) -> Result<Self> {
        let mut spawned: Option<Child> = None;
        let mut last_autostart_error: Option<String> = None;
        loop {
            let mut attempt = connect_any_endpoint(discovery, retry.connect_deadline(CONNECT_ATTEMPT_TIMEOUT)).await;
            if attempt.is_err() && discovery.autostart {
                // Start (or wait for) the engine, then look again at once.
                match ensure_engine_for_connect(discovery, retry, &mut spawned).await {
                    Ok(()) => {
                        attempt =
                            connect_any_endpoint(discovery, retry.connect_deadline(CONNECT_ATTEMPT_TIMEOUT)).await;
                    }
                    // Spawn failure or an engine that died at startup: waiting
                    // longer cannot help, so surface it now with its context.
                    Err(err) if err.is::<AutostartFailed>() => return Err(err),
                    Err(err) => last_autostart_error = Some(format!("{err:#}")),
                }
            }
            let err = match attempt {
                Ok((stream, socket_path)) => {
                    return Ok(Self::from_stream(stream, socket_path, Some(discovery.clone())));
                }
                Err(err) => err,
            };
            if !is_unreachable_kind(err.kind()) {
                return Err(anyhow::Error::new(err)
                    .context(format!("failed to connect to engine socket {}", discovery.socket_path)));
            }
            let mut last_error = err.to_string();
            if let Some(autostart) = &last_autostart_error {
                last_error = format!("{last_error}; engine autostart did not help: {autostart}");
            }
            if !retry.backoff(&discovery.socket_path, "connect", &last_error).await {
                return Err(anyhow::Error::new(
                    retry.unreachable(&discovery.socket_path, &last_error),
                ));
            }
        }
    }

    /// Connect directly to a socket path without autostart or retry logic.
    pub async fn connect_socket(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed to connect to engine socket {socket_path}"))?;
        Ok(Self::from_stream(stream, socket_path.to_owned(), None))
    }

    fn from_stream(stream: UnixStream, socket_path: String, reconnect: Option<Discovery>) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half).lines(),
            writer: write_half,
            next_request_id: AtomicU64::new(1),
            socket_path,
            reconnect,
        }
    }

    /// Send a request and wait for the matching response by `request_id`.
    ///
    /// If the connection fails, a client from [`connect`](Self::connect)
    /// recovers per the policy in [`Discovery::retry`]:
    ///
    /// * the request provably never reached the socket (the first write
    ///   failed with zero bytes written) → reconnect and send it again,
    ///   whatever the request is;
    /// * it may have been delivered (a reply never came, or a write was
    ///   partial or its flush failed) → resend only if [`replay_safety_with`]
    ///   says a second delivery is harmless *at the moment of the resend*,
    ///   i.e. after the reconnect wait; otherwise fail with
    ///   [`OutcomeUnknown`]. "May have been delivered" is sticky across
    ///   attempts.
    ///
    /// A replayed guarded create that the engine's duplicate guard refuses
    /// was in all likelihood applied by the earlier delivery; it is reported
    /// as the existing item (`WorkItemCreated`) rather than as a conflict.
    pub async fn send_request(&mut self, request: &FrontendRequest) -> Result<FrontendEvent> {
        let mut retry: Option<RetryState> = None;
        // Sticky across attempts: when the first attempt that may have
        // reached the engine started. A later `NotSent` must not forget it.
        let mut first_possible_delivery: Option<Instant> = None;
        let mut replayed = false;
        loop {
            let attempt_started = Instant::now();
            let error = match self.exchange(request).await {
                Ok(event) => {
                    return self
                        .settle_reply(request, event, first_possible_delivery, replayed)
                        .await;
                }
                Err(Exchange::Fatal(err)) => return Err(err),
                Err(Exchange::NotSent(err)) => err,
                Err(Exchange::Dropped(err)) => {
                    first_possible_delivery.get_or_insert(attempt_started);
                    err
                }
            };

            let recoverable = self.reconnect.as_ref().filter(|d| d.retry.is_enabled());
            let Some(discovery) = recoverable.cloned() else {
                // No recovery configured: report what happened. A drop
                // after send is still an unknown outcome, never a bare I/O
                // error that reads like "nothing happened".
                return Err(if first_possible_delivery.is_some() {
                    self.outcome_unknown(request, &error)
                } else {
                    error
                });
            };
            let safety = replay_safety_with(request, discovery.duplicate_guard_window);
            let window_closed = |at: Option<Instant>| at.is_some_and(|at| !safety.allows_replay_after(at.elapsed()));
            if window_closed(first_possible_delivery) {
                return Err(self.outcome_unknown(request, &error));
            }
            // Giving up on a possibly-delivered guarded create is an unknown
            // outcome, not "nothing was sent".
            let gave_up = |this: &Self, err: anyhow::Error| {
                if first_possible_delivery.is_some() && matches!(safety, ReplaySafety::Within(_)) {
                    this.outcome_unknown(request, &error)
                } else {
                    err
                }
            };

            let retry = retry.get_or_insert_with(|| RetryState::new(&discovery.retry));
            if let (Some(at), ReplaySafety::Within(window)) = (first_possible_delivery, safety) {
                // The reconnect wait must not outlast the duplicate guard.
                retry.limit_total(retry.elapsed() + window.saturating_sub(at.elapsed()));
            }
            let detail = format!("{error:#}");
            if !retry.backoff(&self.socket_path, "connection dropped", &detail).await {
                let unreachable = anyhow::Error::new(retry.unreachable(&self.socket_path, &detail));
                return Err(gave_up(self, unreachable));
            }
            let fresh = match Self::connect_within(&discovery, retry).await {
                Ok(fresh) => fresh,
                Err(err) if err.is::<EngineUnreachable>() => return Err(gave_up(self, err)),
                Err(err) => return Err(err),
            };
            self.reader = fresh.reader;
            self.writer = fresh.writer;
            self.socket_path = fresh.socket_path;
            // Re-evaluate at resend time: the reconnect may have taken long
            // enough to close the replay window.
            if window_closed(first_possible_delivery) {
                return Err(self.outcome_unknown(request, &error));
            }
            replayed = first_possible_delivery.is_some();
        }
    }

    /// Turn the reply to a possibly-replayed request into the caller's
    /// result. A replayed create that the duplicate guard refuses almost
    /// certainly found its own earlier delivery; fetch that item and report
    /// it as created instead of a conflict whose `--force-duplicate` hint
    /// would invite exactly the twin the replay guard exists to prevent.
    async fn settle_reply(
        &mut self,
        request: &FrontendRequest,
        event: FrontendEvent,
        first_possible_delivery: Option<Instant>,
        replayed: bool,
    ) -> Result<FrontendEvent> {
        let (true, Some(sent_at)) = (replayed, first_possible_delivery) else {
            return Ok(event);
        };
        let FrontendEvent::WorkItemDuplicateBlocked {
            existing_id, age_secs, ..
        } = &event
        else {
            return Ok(event);
        };
        // An item older than our first delivery predates this request: a
        // genuine duplicate of someone else's create, reported as usual.
        let age = u64::try_from(*age_secs).unwrap_or(0);
        if age > sent_at.elapsed().as_secs() + 2 {
            return Ok(event);
        }
        let existing_id = existing_id.clone();
        let lookup = FrontendRequest::GetWorkItem {
            id: existing_id.clone(),
        };
        match self.exchange(&lookup).await {
            Ok(FrontendEvent::WorkItemResult { item }) => Ok(FrontendEvent::WorkItemCreated { item }),
            _ => Err(anyhow::anyhow!(
                "the `{}` request was resent after the connection dropped, and the engine reports the item \
                 already exists as {existing_id} (created {age}s ago), most likely from the first delivery. \
                 Look it up with that id; do not create it again.",
                request_name(request),
            )),
        }
    }

    fn outcome_unknown(&self, request: &FrontendRequest, error: &anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(OutcomeUnknown {
            request: request_name(request),
            socket_path: self.socket_path.clone(),
            detail: format!("{error:#}"),
        })
    }

    async fn exchange(&mut self, request: &FrontendRequest) -> Result<FrontendEvent, Exchange> {
        let request_id = format!("client-{}", self.next_request_id.fetch_add(1, Ordering::Relaxed));
        let mut payload = serde_json::to_string(&FrontendRequestEnvelope {
            request_id: request_id.clone(),
            payload: request.clone(),
        })
        .map_err(|err| Exchange::Fatal(err.into()))?;
        payload.push('\n');
        // Write by hand rather than `write_all` so a failure reports how many
        // bytes got out: only a first write that fails with nothing written
        // proves the engine saw none of the request.
        let bytes = payload.as_bytes();
        let mut written = 0;
        while written < bytes.len() {
            let failure = match self.writer.write(&bytes[written..]).await {
                Ok(0) => io::Error::new(io::ErrorKind::WriteZero, "wrote zero bytes to the engine socket"),
                Ok(n) => {
                    written += n;
                    continue;
                }
                Err(err) => err,
            };
            let err = anyhow::Error::new(failure).context("failed to write request to engine");
            return Err(if written == 0 {
                Exchange::NotSent(err)
            } else {
                Exchange::Dropped(err)
            });
        }
        self.writer
            .flush()
            .await
            .map_err(|err| Exchange::Dropped(anyhow::Error::new(err).context("failed to flush request to engine")))?;

        loop {
            let line = match self.reader.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    return Err(Exchange::Dropped(anyhow::anyhow!(
                        "engine closed the socket before returning a response"
                    )));
                }
                Err(err) => {
                    return Err(Exchange::Dropped(
                        anyhow::Error::new(err).context("failed to read engine response"),
                    ));
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let envelope: FrontendEventEnvelope = serde_json::from_str(&line)
                .with_context(|| format!("failed to decode engine event: {line}"))
                .map_err(Exchange::Fatal)?;
            if envelope.request_id.as_deref() == Some(request_id.as_str()) {
                return Ok(envelope.payload);
            }
        }
    }
}

/// One connect attempt across the discovery endpoints (primary, then the
/// legacy fallback). On failure returns the *primary* endpoint's error, the
/// one the user can act on.
/// Every endpoint's connect is bounded by `deadline` (and by
/// [`CONNECT_ATTEMPT_TIMEOUT`]); once it passes, remaining endpoints are skipped.
async fn connect_any_endpoint(discovery: &Discovery, deadline: Instant) -> io::Result<(UnixStream, String)> {
    let mut primary_error = None;
    for (socket_path, _) in discovery.endpoint_candidates() {
        if primary_error.is_some() && Instant::now() >= deadline {
            break;
        }
        match connect_until(socket_path, deadline).await {
            Ok(stream) => return Ok((stream, socket_path.to_owned())),
            Err(err) => primary_error.get_or_insert(err),
        };
    }
    Err(primary_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no engine endpoint configured")))
}

/// `UnixStream::connect` that cannot outlive `deadline` or the per-attempt
/// cap. A listener whose accept is stalled would otherwise hold a bare
/// connect forever.
async fn connect_until(socket_path: &str, deadline: Instant) -> io::Result<UnixStream> {
    let budget = deadline
        .saturating_duration_since(Instant::now())
        .clamp(MIN_CONNECT_TIMEOUT, CONNECT_ATTEMPT_TIMEOUT);
    match timeout(budget, UnixStream::connect(socket_path)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("connect to {socket_path} timed out after {:.1}s", budget.as_secs_f64()),
        )),
    }
}

/// Autostart step inside the connect loop: wait for an engine that is
/// already starting/restarting, or start one — but start at most one per
/// `connect` call (`spawned`), so a slow start is waited on, not duplicated.
async fn ensure_engine_for_connect(
    discovery: &Discovery,
    retry: &RetryState,
    spawned: &mut Option<Child>,
) -> Result<()> {
    let wait = if retry.policy().is_enabled() {
        let remaining = retry.policy().max_wait.saturating_sub(retry.elapsed());
        discovery.start_timeout.min(remaining.max(Duration::from_millis(250)))
    } else {
        discovery.start_timeout
    };
    ensure_engine_running_with(discovery, spawned.is_none(), wait, spawned).await
}

impl BossClient {
    /// Ask the running engine for its version identifiers. Returns
    /// `(version, git_sha, build_time, binary_fingerprint)`. The
    /// `binary_fingerprint` is the most reliable signal for detecting
    /// whether the running engine matches an expected binary — see
    /// `boss_engine::build_info::binary_fingerprint` for the algorithm.
    pub async fn get_engine_version(&mut self) -> Result<(String, String, String, String)> {
        let event = self
            .send_request(&boss_protocol::FrontendRequest::GetEngineVersion)
            .await?;
        match event {
            boss_protocol::FrontendEvent::EngineVersionResult {
                version,
                git_sha,
                build_time,
                binary_fingerprint,
            } => Ok((version, git_sha, build_time, binary_fingerprint)),
            other => anyhow::bail!("unexpected response to GetEngineVersion: {:?}", other),
        }
    }
}

pub async fn engine_socket_reachable(socket_path: &str) -> bool {
    connect_until(socket_path, Instant::now() + CONNECT_ATTEMPT_TIMEOUT)
        .await
        .is_ok()
}

pub async fn discover_running_engine(discovery: &Discovery) -> Option<RunningEngine> {
    discover_running_engine_until(discovery, Instant::now() + CONNECT_ATTEMPT_TIMEOUT).await
}

async fn discover_running_engine_until(discovery: &Discovery, deadline: Instant) -> Option<RunningEngine> {
    for (socket_path, pid_file_path) in discovery.endpoint_candidates() {
        let Ok(stream) = connect_until(socket_path, deadline).await else {
            continue;
        };
        let peer_pid = stream
            .peer_cred()
            .ok()
            .and_then(|credentials| credentials.pid())
            .and_then(|pid| u32::try_from(pid).ok());
        // Peer credentials are direct, unforgeable proof of which process
        // owns the socket we just connected to; the pid file is the
        // deletable, potentially stale artefact this discovery step
        // otherwise leans on. Prefer the peer pid and only fall back to
        // the file when peer credentials aren't available.
        let pid = peer_pid.or_else(|| running_engine_pid(pid_file_path));
        return Some(RunningEngine {
            socket_path: socket_path.to_owned(),
            pid_file_path: pid_file_path.to_owned(),
            pid,
        });
    }
    None
}

pub async fn wait_for_socket(socket_path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if engine_socket_reachable(socket_path).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

pub fn running_engine_pid(pid_file_path: &str) -> Option<u32> {
    let pid = read_pid_file(pid_file_path)?;
    let status = Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if status.success() {
        Some(pid)
    } else {
        let _ = std::fs::remove_file(pid_file_path);
        None
    }
}

pub fn read_pid_file(pid_file_path: &str) -> Option<u32> {
    let content = std::fs::read_to_string(pid_file_path).ok()?;
    content.trim().parse().ok()
}

pub async fn ensure_engine_running(discovery: &Discovery) -> Result<()> {
    ensure_engine_running_with(discovery, true, discovery.start_timeout, &mut None).await
}

/// [`ensure_engine_running`] with the pieces `BossClient::connect` needs to
/// call it repeatedly inside a backoff loop: `allow_spawn` says whether this
/// call may start an engine process (the loop passes `false` once it has
/// already started one), `wait` bounds each readiness wait, and `spawned` is
/// holds the process when this call starts one.
async fn ensure_engine_running_with(
    discovery: &Discovery,
    allow_spawn: bool,
    wait: Duration,
    spawned: &mut Option<Child>,
) -> Result<()> {
    let deadline = Instant::now() + wait;
    if discover_running_engine_until(discovery, deadline).await.is_some() {
        return Ok(());
    }

    // A pid file naming a live, recognizably-engine process is worth
    // waiting on rather than starting a competitor. But `running_engine_pid`
    // only checks `kill -0`, so a stale pid file whose number has been
    // reused by any unrelated live process must not veto autostart forever
    // — gate on `is_likely_engine_process` too, and fall through to
    // starting a fresh engine when that check fails. The events-socket
    // guard in `serve_with_merge_probe` still fails closed if a real engine
    // turns out to be alive after all.
    if let Some((pid, _)) = discovery
        .endpoint_candidates()
        .into_iter()
        .find_map(|(_, pid_path)| running_engine_pid(pid_path).map(|pid| (pid, pid_path)))
        .filter(|(pid, _)| is_likely_engine_process(*pid))
    {
        if wait_for_discovered_engine_until(discovery, deadline, spawned).await? {
            return Ok(());
        }
        bail!(
            "boss engine pid file points to pid {pid}, but socket {} never became ready",
            discovery.socket_path
        );
    }

    if !allow_spawn {
        // We already started an engine for this call and it has not
        // published its socket yet: keep waiting, do not start another.
        if wait_for_discovered_engine_until(discovery, deadline, spawned).await? {
            return Ok(());
        }
        bail!(
            "the engine started for this command has not yet published socket {}",
            discovery.socket_path
        );
    }

    let child = start_engine_process(discovery).map_err(|err| AutostartFailed(format!("{err:#}")))?;
    *spawned = Some(child);
    if wait_for_discovered_engine_until(discovery, Instant::now() + wait, spawned).await? {
        return Ok(());
    }

    bail!(
        "boss engine did not become ready at {} within {} seconds",
        discovery.socket_path,
        wait.as_secs()
    )
}

/// The engine process this call started failed in a way waiting cannot fix:
/// it could not be spawned, or it exited before publishing its socket.
#[derive(Debug)]
struct AutostartFailed(String);

impl std::fmt::Display for AutostartFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AutostartFailed {}

/// Poll for a reachable engine until `deadline`. If `child` is the engine this
/// call started exits first, report its status unless another engine is starting.
async fn wait_for_discovered_engine_until(
    discovery: &Discovery,
    deadline: Instant,
    child: &mut Option<Child>,
) -> Result<bool> {
    while Instant::now() < deadline {
        if discover_running_engine_until(discovery, deadline).await.is_some() {
            return Ok(true);
        }
        if let Some(child) = child
            && let Ok(Some(status)) = child.try_wait()
        {
            // A concurrent autostart may have won ownership while our child
            // exited. Check again before reporting a startup failure.
            if discover_running_engine_until(discovery, deadline).await.is_some() {
                return Ok(true);
            }
            let another_engine = discovery.endpoint_candidates().into_iter().any(|(_, path)| {
                running_engine_pid(path)
                    .is_some_and(|pid| pid != child.id() && is_likely_engine_process(pid))
            });
            if another_engine {
                sleep(Duration::from_millis(100)).await;
                continue;
            }
            return Err(anyhow::Error::new(AutostartFailed(format!(
                "the engine process started for this command exited before becoming ready ({status}); \
                 socket {} was never published",
                discovery.socket_path
            ))));
        }
        sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// Stop the running engine. Preferred path is the token-authenticated
/// `Shutdown` RPC on the frontend socket — same authority the macOS
/// app uses (issue #705). Falls back to `SIGTERM` only when the RPC
/// path can't be exercised (no token file, socket unreachable,
/// engine refuses the token). The SIGTERM fallback exists so a
/// developer recovering from a wedged engine on a non-standard layout
/// still has a recoverable kill switch; the everyday "restart engine"
/// case always takes the RPC.
///
/// Async because the RPC path needs to share the caller's tokio
/// runtime — building a nested runtime here panics with "Cannot
/// start a runtime from within a runtime" (#720).
pub async fn stop_engine(discovery: &Discovery) -> Result<EngineStopOutcome> {
    let running = discover_running_engine(discovery).await;
    let pid_only = discovery
        .endpoint_candidates()
        .into_iter()
        .find_map(|(_, pid_path)| running_engine_pid(pid_path).map(|pid| (pid, pid_path.to_owned())));

    let Some(running) = running else {
        if let Some((pid, pid_file_path)) = pid_only {
            if !is_likely_engine_process(pid) {
                bail!("pid file {pid_file_path} names pid {pid}, which is not a recognized Boss engine");
            }
            terminate_pid(pid)?;
            wait_for_process_exit(pid, Duration::from_secs(8)).await?;
            clear_pid_file_if_owned(&pid_file_path, pid);
            return Ok(EngineStopOutcome::Stopped);
        }
        return Ok(EngineStopOutcome::AlreadyStopped);
    };

    let rpc_error = match try_shutdown_via_rpc_at(&discovery.control_token_path, Some(&running.socket_path)).await {
        Ok(()) => {
            if wait_for_socket_close(&running.socket_path, Duration::from_secs(8)).await {
                if let Some(pid) = running.pid {
                    clear_pid_file_if_owned(&running.pid_file_path, pid);
                }
                return Ok(EngineStopOutcome::Stopped);
            }
            Some(anyhow::anyhow!(
                "engine accepted shutdown but socket {} remained reachable after 8 seconds",
                running.socket_path
            ))
        }
        Err(err) => Some(err),
    };

    let Some(pid) = running.pid else {
        bail!(
            "engine is reachable at {}, but graceful shutdown failed and no pid is available: {:#}",
            running.socket_path,
            rpc_error.expect("rpc error is always recorded")
        );
    };
    if !is_likely_engine_process(pid) {
        bail!(
            "engine is reachable at {}, but graceful shutdown failed and socket peer pid {pid} is not a recognized Boss engine: {:#}",
            running.socket_path,
            rpc_error.expect("rpc error is always recorded")
        );
    }

    tracing::warn!(
        ?rpc_error,
        pid,
        socket_path = running.socket_path,
        "stop_engine: rpc shutdown unavailable; falling back to SIGTERM",
    );
    terminate_pid(pid)?;
    wait_for_process_exit(pid, Duration::from_secs(8)).await?;
    if !wait_for_socket_close(&running.socket_path, Duration::from_secs(2)).await {
        bail!(
            "boss engine pid {pid} exited but socket {} remains reachable",
            running.socket_path
        );
    }
    clear_pid_file_if_owned(&running.pid_file_path, pid);
    Ok(EngineStopOutcome::Stopped)
}

/// Token-path-injected variant of [`try_shutdown_via_rpc`]. Kept
/// separate so tests can drive the RPC without mutating the global
/// `BOSS_ENGINE_CONTROL_TOKEN_PATH` env var.
async fn try_shutdown_via_rpc_at(token_path: &Path, expected_socket_path: Option<&str>) -> Result<()> {
    let raw = std::fs::read_to_string(token_path)
        .with_context(|| format!("failed to read engine-control token file {}", token_path.display()))?;
    let parsed: ControlTokenFile = serde_json::from_str(&raw)
        .with_context(|| format!("malformed engine-control token file {}", token_path.display()))?;
    if let Some(expected) = expected_socket_path
        && !same_path(Path::new(&parsed.socket_path), Path::new(expected))
    {
        bail!(
            "engine-control token names socket {}, not the reachable socket {expected}",
            parsed.socket_path
        );
    }

    let mut client = BossClient::connect_socket(&parsed.socket_path).await?;
    let event = client
        .send_request(&FrontendRequest::Shutdown {
            token: parsed.token.clone(),
        })
        .await?;
    match event {
        FrontendEvent::ShutdownAccepted => Ok(()),
        FrontendEvent::ShutdownRejected { reason } => {
            bail!("engine rejected shutdown rpc: {reason}");
        }
        other => bail!("unexpected response to Shutdown rpc: {:?}", other),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

async fn wait_for_socket_close(socket_path: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !engine_socket_reachable(socket_path).await {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    !engine_socket_reachable(socket_path).await
}

fn terminate_pid(pid: u32) -> Result<()> {
    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("failed to invoke /bin/kill")?;
    if !status.success() {
        bail!("failed to stop boss engine pid {pid}");
    }
    Ok(())
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !process_is_running(pid) {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "boss engine pid {pid} did not exit within {} seconds",
        timeout.as_secs()
    )
}

fn process_is_running(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn clear_pid_file_if_owned(pid_file_path: &str, pid: u32) {
    if read_pid_file(pid_file_path) == Some(pid) {
        let _ = std::fs::remove_file(pid_file_path);
    }
}

fn is_likely_engine_process(pid: u32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    is_likely_engine_command(&command)
}

fn is_likely_engine_command(command: &str) -> bool {
    let executable_matches = command
        .split_whitespace()
        .next()
        .and_then(|executable| Path::new(executable).file_name())
        .is_some_and(|name| name == ENGINE_BINARY_NAME);
    executable_matches || command.contains(BAZEL_ENGINE_RELPATH) || command.contains(ENGINE_BINARY_TARGET)
}

/// Minimal on-disk view of the engine-control token file. Kept in
/// this crate (rather than reused from `boss-engine`) so the CLI's
/// dep graph doesn't pull the full engine.
#[derive(Debug, serde::Deserialize)]
struct ControlTokenFile {
    token: String,
    socket_path: String,
}

/// Default token path, mirroring `boss_engine::engine_control::default_token_path`.
/// Duplicated rather than re-exported so the CLI does not depend on
/// the engine crate.
pub fn default_control_token_path() -> Option<PathBuf> {
    const ENV: &str = "BOSS_ENGINE_CONTROL_TOKEN_PATH";
    if let Some(override_path) = std::env::var_os(ENV) {
        let trimmed = override_path.to_string_lossy().trim().to_owned();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss/engine-control.token"))
}

fn start_engine_process(discovery: &Discovery) -> Result<Child> {
    // The spawn boundary: no caller (autostart, `boss engine start`, a
    // future one) can launch an engine from a worker session.
    if discovery.worker_environment {
        bail!(
            "refusing to start a Boss engine from a worker session ({WORKER_RUN_ENV} is set); \
             workers must never start an engine"
        );
    }
    Command::new(&discovery.engine.program)
        .args(&discovery.engine.args)
        .current_dir(&discovery.launch_directory)
        .env("BOSS_ENGINE_PID_PATH", &discovery.pid_file_path)
        .env("BOSS_SOCKET_PATH", &discovery.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start engine using `{}` (resolved via {}).\nResolution chain (highest priority first):\n{}\nSet BOSS_ENGINE_BIN to an explicit engine binary path, or run `bazel build {ENGINE_BINARY_TARGET}` so the bazel-bin lookup succeeds.",
                format_engine_command(&discovery.engine.program, &discovery.engine.args),
                discovery.engine.source,
                format_resolution_chain(&discovery.engine.attempted),
            )
        })
}

fn resolve_launch_directory() -> Result<PathBuf> {
    if let Some(workspace) = locate_bazel_workspace_root() {
        return Ok(workspace);
    }
    std::env::current_dir().context("failed to resolve current directory")
}

/// The engine's output identity comes from the same Bazel definition that
/// names and bundles the executable. This prevents the resolver from drifting
/// from the filename installed in `Boss.app`.
const ENGINE_BINARY_NAME: &str = env!("BOSS_ENGINE_BINARY_NAME");
const BAZEL_ENGINE_RELPATH: &str = env!("BOSS_ENGINE_BINARY_BAZEL_RELPATH");
const ENGINE_BINARY_TARGET: &str = env!("BOSS_ENGINE_BINARY_TARGET");

fn resolve_engine_command(socket_path: &str) -> Result<EngineCommand> {
    let input = EngineResolverInput {
        env_cmd: non_empty_env("BOSS_ENGINE_CMD"),
        env_bin: non_empty_env("BOSS_ENGINE_BIN"),
        workspace_root: locate_bazel_workspace_root(),
        current_exe: std::env::current_exe().ok(),
    };
    resolve_engine_command_with(socket_path, &input)
}

/// Pure resolver used by [`resolve_engine_command`] and tests.
///
/// Resolution order (highest priority first):
///   1. `BOSS_ENGINE_CMD` — full custom command (shell-split).
///   2. `BOSS_ENGINE_BIN` — explicit binary path; default args appended.
///   3. The Bazel-built engine under the workspace root.
///   4. The engine sibling next to the running executable — covers the
///      installed app bundle and `bazel run` runfiles layouts.
///   5. Bare engine executable on `$PATH` (current default; fails loudly if
///      the binary isn't installed).
pub fn resolve_engine_command_with(socket_path: &str, input: &EngineResolverInput) -> Result<EngineCommand> {
    let mut attempted = Vec::new();

    if let Some(value) = input.env_cmd.as_deref() {
        attempted.push(format!("BOSS_ENGINE_CMD={value}"));
        let parts = shlex::split(value).with_context(|| format!("failed to parse BOSS_ENGINE_CMD: {value}"))?;
        let Some((program, args)) = parts.split_first() else {
            bail!("BOSS_ENGINE_CMD resolved to an empty command");
        };
        return Ok(EngineCommand {
            program: program.clone(),
            args: args.to_vec(),
            source: "BOSS_ENGINE_CMD env var".to_owned(),
            attempted,
        });
    }
    attempted.push("BOSS_ENGINE_CMD env var (unset)".to_owned());

    if let Some(value) = input.env_bin.as_deref() {
        attempted.push(format!("BOSS_ENGINE_BIN={value}"));
        return Ok(EngineCommand {
            program: value.to_owned(),
            args: default_engine_args(socket_path),
            source: "BOSS_ENGINE_BIN env var".to_owned(),
            attempted,
        });
    }
    attempted.push("BOSS_ENGINE_BIN env var (unset)".to_owned());

    if let Some(workspace) = input.workspace_root.as_deref() {
        let candidate = workspace.join(BAZEL_ENGINE_RELPATH);
        if candidate.is_file() {
            attempted.push(format!("bazel-bin lookup hit {}", candidate.display()));
            return Ok(EngineCommand {
                program: candidate.to_string_lossy().into_owned(),
                args: default_engine_args(socket_path),
                source: format!("bazel-bin ({})", candidate.display()),
                attempted,
            });
        }
        attempted.push(format!(
            "bazel-bin lookup miss at {} (run `bazel build {ENGINE_BINARY_TARGET}`)",
            candidate.display()
        ));
    } else {
        attempted.push("bazel-bin lookup skipped (no workspace root found)".to_owned());
    }

    if let Some(exe) = input.current_exe.as_deref() {
        if let Some((program, candidate)) = sibling_engine_binary(exe) {
            attempted.push(format!("sibling-of-exe hit {}", candidate.display()));
            return Ok(EngineCommand {
                program,
                args: default_engine_args(socket_path),
                source: format!("sibling of {}", exe.display()),
                attempted,
            });
        }
        attempted.push(format!("sibling-of-exe miss next to {}", exe.display()));
    } else {
        attempted.push("sibling-of-exe skipped (current_exe unavailable)".to_owned());
    }

    attempted.push(format!("PATH lookup of `{ENGINE_BINARY_NAME}`"));
    Ok(EngineCommand {
        program: ENGINE_BINARY_NAME.to_owned(),
        args: default_engine_args(socket_path),
        source: format!("PATH lookup of `{ENGINE_BINARY_NAME}`"),
        attempted,
    })
}

fn non_empty_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn default_engine_args(socket_path: &str) -> Vec<String> {
    vec!["--socket-path".to_owned(), socket_path.to_owned()]
}

fn sibling_engine_binary(exe: &Path) -> Option<(String, PathBuf)> {
    let dir = exe.parent()?;
    let candidate = dir.join(ENGINE_BINARY_NAME);
    candidate.is_file().then(|| {
        let path_str = candidate.to_string_lossy().into_owned();
        (path_str, candidate)
    })
}

fn locate_bazel_workspace_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BUILD_WORKSPACE_DIRECTORY") {
        let candidate = PathBuf::from(path);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    walk_to_workspace_root(&cwd)
}

fn walk_to_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if is_bazel_workspace(current) {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn is_bazel_workspace(dir: &Path) -> bool {
    dir.join("MODULE.bazel").exists() || dir.join("WORKSPACE").exists() || dir.join("WORKSPACE.bazel").exists()
}

fn format_engine_command(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_resolution_chain(attempted: &[String]) -> String {
    if attempted.is_empty() {
        return "(none)".to_owned();
    }
    attempted
        .iter()
        .enumerate()
        .map(|(idx, step)| format!("  {}. {step}", idx + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_process_validation_matches_engine_executables() {
        assert!(is_likely_engine_command(
            "/Applications/Boss.app/Contents/Resources/bin/engine --socket-path /tmp/test.sock"
        ));
        assert!(is_likely_engine_command(
            "/workspace/bazel-bin/tools/boss/engine/core/engine --socket-path /tmp/test.sock"
        ));
        assert!(is_likely_engine_command(
            "bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/test.sock"
        ));
    }

    #[test]
    fn engine_process_validation_rejects_incidental_engine_text() {
        assert!(!is_likely_engine_command("/usr/bin/python3 search_engine_worker.py"));
        assert!(!is_likely_engine_command("/usr/local/bin/render-engine-helper --serve"));
    }

    fn make_workspace_with_engine(tmp: &tempfile::TempDir) -> PathBuf {
        let root = tmp.path();
        std::fs::write(root.join("MODULE.bazel"), "module(name = \"test\")\n").unwrap();
        let engine_bin = root.join(BAZEL_ENGINE_RELPATH);
        let engine_dir = engine_bin.parent().unwrap();
        std::fs::create_dir_all(engine_dir).unwrap();
        std::fs::write(&engine_bin, b"#!/bin/sh\nexit 0\n").unwrap();
        engine_bin
    }

    #[test]
    fn env_cmd_wins_over_everything() {
        let tmp = tempfile::tempdir().unwrap();
        make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: Some("/custom/cmd --flag".to_owned()),
            env_bin: Some("/should-not-win/engine".to_owned()),
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/custom/cmd");
        assert_eq!(cmd.args, vec!["--flag".to_owned()]);
        assert_eq!(cmd.source, "BOSS_ENGINE_CMD env var");
    }

    #[test]
    fn env_bin_wins_over_bazel_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: Some("/explicit/engine".to_owned()),
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/explicit/engine");
        assert_eq!(cmd.args, vec!["--socket-path".to_owned(), "/tmp/sock".to_owned(),]);
        assert_eq!(cmd.source, "BOSS_ENGINE_BIN env var");
    }

    #[test]
    fn bazel_bin_wins_over_path_when_built() {
        let tmp = tempfile::tempdir().unwrap();
        let engine_bin = make_workspace_with_engine(&tmp);
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, engine_bin.to_string_lossy());
        assert!(
            cmd.source.starts_with("bazel-bin"),
            "expected bazel-bin source, got {}",
            cmd.source
        );
    }

    #[test]
    fn falls_back_to_path_when_engine_not_built() {
        let tmp = tempfile::tempdir().unwrap();
        // Workspace exists but the Bazel engine has not been built yet.
        std::fs::write(tmp.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(tmp.path().to_path_buf()),
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, ENGINE_BINARY_NAME);
        assert_eq!(cmd.source, format!("PATH lookup of `{ENGINE_BINARY_NAME}`"));
        // The chain must mention that bazel-bin was attempted but missed,
        // so the error message guides the user to `bazel build`.
        let chain_text = cmd.attempted.join("\n");
        assert!(
            chain_text.contains("bazel-bin lookup miss"),
            "expected bazel-bin miss to be reported in chain, got: {chain_text}"
        );
        assert!(chain_text.contains("PATH lookup"));
    }

    #[test]
    fn falls_back_to_path_when_no_workspace() {
        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: None,
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, ENGINE_BINARY_NAME);
        assert_eq!(cmd.source, format!("PATH lookup of `{ENGINE_BINARY_NAME}`"));
    }

    #[test]
    fn empty_env_cmd_is_ignored() {
        // `BOSS_ENGINE_CMD=""` (or whitespace) should not poison resolution.
        let input = EngineResolverInput {
            env_cmd: None, // non_empty_env strips this in production
            env_bin: Some("/from-bin/engine".to_owned()),
            workspace_root: None,
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/from-bin/engine");
    }

    #[test]
    fn walk_to_workspace_root_finds_module_bazel() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("MODULE.bazel"), "").unwrap();
        let nested = tmp.path().join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        let found = walk_to_workspace_root(&nested).unwrap();
        // Compare canonicalized paths to be robust against /var vs /private/var
        // on macOS where TMPDIR resolves through a symlink.
        assert_eq!(
            std::fs::canonicalize(&found).unwrap(),
            std::fs::canonicalize(tmp.path()).unwrap(),
        );
    }

    /// Regression for #720: pre-fix, `try_shutdown_via_rpc` built a
    /// `new_current_thread` runtime and called `block_on` inside it,
    /// which panics when invoked from a thread that already drives a
    /// tokio runtime (every caller — the CLI's `#[tokio::main]`).
    ///
    /// This test pins the function as `async` by calling it through
    /// `.await` from a `#[tokio::test]` and verifying that it
    /// returns an `Err` (because the socket is unreachable) instead
    /// of panicking. With the bug present, the test panics inside
    /// `block_on`; with the fix, it returns `Err`.
    #[tokio::test]
    async fn try_shutdown_via_rpc_at_does_not_panic_inside_tokio_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let token_path = tmp.path().join("engine-control.token");
        let socket_path = tmp.path().join("unreachable.sock");
        let token_json = serde_json::json!({
            "token": "abcd0123",
            "socket_path": socket_path.to_string_lossy(),
        });
        std::fs::write(&token_path, token_json.to_string()).unwrap();

        let result = try_shutdown_via_rpc_at(&token_path, None).await;
        let err = result.expect_err("connecting to a non-existent socket must fail");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to connect to engine socket"),
            "expected socket connect failure, got: {chain}",
        );
    }

    #[tokio::test]
    async fn discovery_falls_back_to_legacy_socket_without_a_pid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let primary_socket = tmp.path().join("state-root.sock");
        let legacy_socket = tmp.path().join("legacy.sock");
        let legacy_listener = tokio::net::UnixListener::bind(&legacy_socket).unwrap();
        let accept = tokio::spawn(async move {
            let _ = legacy_listener.accept().await;
        });
        let discovery = Discovery {
            socket_path: primary_socket.to_string_lossy().into_owned(),
            pid_file_path: tmp.path().join("state-root.pid").to_string_lossy().into_owned(),
            legacy_socket_path: Some(legacy_socket.to_string_lossy().into_owned()),
            legacy_pid_file_path: Some(tmp.path().join("missing-legacy.pid").to_string_lossy().into_owned()),
            control_token_path: tmp.path().join("engine-control.token"),
            autostart: false,
            engine: EngineCommand {
                program: "unused".into(),
                args: Vec::new(),
                source: "test".into(),
                attempted: Vec::new(),
            },
            launch_directory: tmp.path().to_path_buf(),
            start_timeout: Duration::from_secs(1),
            retry: RetryPolicy::disabled(),
            worker_environment: false,
            duplicate_guard_window: DUPLICATE_GUARD_WINDOW,
        };

        let running = discover_running_engine(&discovery)
            .await
            .expect("legacy listener is proof of engine liveness");
        assert_eq!(running.socket_path, legacy_socket.to_string_lossy());
        assert!(
            running.pid.is_some(),
            "peer credentials supply a pid when the pid file is absent"
        );
        accept.await.unwrap();
    }

    #[tokio::test]
    async fn stop_reports_when_no_engine_was_running() {
        let tmp = tempfile::tempdir().unwrap();
        let discovery = Discovery {
            socket_path: tmp.path().join("missing.sock").to_string_lossy().into_owned(),
            pid_file_path: tmp.path().join("missing.pid").to_string_lossy().into_owned(),
            legacy_socket_path: None,
            legacy_pid_file_path: None,
            control_token_path: tmp.path().join("missing.token"),
            autostart: false,
            engine: EngineCommand {
                program: "unused".into(),
                args: Vec::new(),
                source: "test".into(),
                attempted: Vec::new(),
            },
            launch_directory: tmp.path().to_path_buf(),
            start_timeout: Duration::from_secs(1),
            retry: RetryPolicy::disabled(),
            worker_environment: false,
            duplicate_guard_window: DUPLICATE_GUARD_WINDOW,
        };

        assert_eq!(
            stop_engine(&discovery).await.unwrap(),
            EngineStopOutcome::AlreadyStopped
        );
    }

    #[test]
    fn sibling_of_exe_resolves_engine_from_installed_app_bundle() {
        // Model the installed bundle layout rather than a Bazel runfiles
        // tree: a bundled `boss` lives beside the engine in Resources/bin.
        // The resolver and `macos_application` consume the same Bazel source
        // for ENGINE_BINARY_NAME, so renaming the build output changes both.
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("Boss.app/Contents/Resources/bin");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let fake_exe = exe_dir.join("boss");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let sibling = exe_dir.join(ENGINE_BINARY_NAME);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();

        // Workspace root with NO bazel-bin engine built.
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();

        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(ws.path().to_path_buf()),
            current_exe: Some(fake_exe.clone()),
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, sibling.to_string_lossy());
        assert!(
            cmd.source.starts_with("sibling of"),
            "expected sibling source, got {}",
            cmd.source
        );
        assert_eq!(cmd.args, vec!["--socket-path".to_owned(), "/tmp/sock".to_owned()]);
    }

    #[test]
    fn bazel_bin_miss_falls_through_to_sibling_before_path() {
        // Ordering guarantee: with a workspace whose bazel-bin engine is
        // NOT built and a valid sibling next to the exe, the sibling wins
        // and the PATH fallback is never reached. The attempted chain must
        // record the bazel-bin miss ahead of the sibling hit.
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("runfiles");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let fake_exe = exe_dir.join("boss");
        std::fs::write(&fake_exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let sibling = exe_dir.join(ENGINE_BINARY_NAME);
        std::fs::write(&sibling, b"#!/bin/sh\nexit 0\n").unwrap();

        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();

        let input = EngineResolverInput {
            env_cmd: None,
            env_bin: None,
            workspace_root: Some(ws.path().to_path_buf()),
            current_exe: Some(fake_exe.clone()),
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, sibling.to_string_lossy());
        assert_ne!(cmd.program, ENGINE_BINARY_NAME, "must not fall through to PATH");

        let chain_text = cmd.attempted.join("\n");
        assert!(
            chain_text.contains("bazel-bin lookup miss"),
            "expected bazel-bin miss in chain, got: {chain_text}"
        );
        assert!(
            chain_text.contains("sibling-of-exe hit"),
            "expected sibling hit in chain, got: {chain_text}"
        );
        assert!(
            !chain_text.contains("PATH lookup"),
            "PATH fallback must not be reached, got: {chain_text}"
        );
        let miss_idx = chain_text.find("bazel-bin lookup miss").unwrap();
        let hit_idx = chain_text.find("sibling-of-exe hit").unwrap();
        assert!(miss_idx < hit_idx, "bazel-bin miss must precede sibling hit");
    }

    #[test]
    fn env_cmd_whitespace_only_is_an_empty_command_error() {
        // A whitespace-only BOSS_ENGINE_CMD shlex-splits to an empty vec.
        let input = EngineResolverInput {
            env_cmd: Some("   ".to_owned()),
            env_bin: None,
            workspace_root: None,
            current_exe: None,
        };
        let err = resolve_engine_command_with("/tmp/sock", &input).expect_err("whitespace-only command must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("resolved to an empty command"),
            "expected empty-command error, got: {chain}"
        );
    }

    #[test]
    fn env_cmd_unbalanced_quote_is_a_parse_error() {
        // An unbalanced quote makes shlex::split return None.
        let input = EngineResolverInput {
            env_cmd: Some("/x/engine \"unterminated".to_owned()),
            env_bin: None,
            workspace_root: None,
            current_exe: None,
        };
        let err = resolve_engine_command_with("/tmp/sock", &input).expect_err("unbalanced quote must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("failed to parse BOSS_ENGINE_CMD"),
            "expected parse error, got: {chain}"
        );
    }

    #[test]
    fn env_cmd_parses_program_and_multiple_args() {
        let input = EngineResolverInput {
            env_cmd: Some("/x/engine --a --b".to_owned()),
            env_bin: None,
            workspace_root: None,
            current_exe: None,
        };
        let cmd = resolve_engine_command_with("/tmp/sock", &input).unwrap();
        assert_eq!(cmd.program, "/x/engine");
        assert_eq!(cmd.args, vec!["--a".to_owned(), "--b".to_owned()]);
        assert_eq!(cmd.source, "BOSS_ENGINE_CMD env var");
    }

    #[test]
    fn format_engine_command_joins_program_and_args_with_spaces() {
        assert_eq!(
            format_engine_command("/x/engine", &["--a".to_owned(), "--b".to_owned()]),
            "/x/engine --a --b"
        );
        // No args -> just the program, no trailing space.
        assert_eq!(format_engine_command(ENGINE_BINARY_NAME, &[]), ENGINE_BINARY_NAME);
    }

    #[test]
    fn format_resolution_chain_numbers_steps_and_handles_empty() {
        assert_eq!(format_resolution_chain(&[]), "(none)");
        let chain = format_resolution_chain(&["first step".to_owned(), "second step".to_owned()]);
        assert_eq!(chain, "  1. first step\n  2. second step");
    }

    #[test]
    fn walk_to_workspace_root_returns_none_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        // No MODULE.bazel/WORKSPACE anywhere along the path inside tmp.
        let nested = tmp.path().join("nope");
        std::fs::create_dir_all(&nested).unwrap();
        // The walker stops at filesystem root if it never finds a marker;
        // this only proves "no marker inside the tmp tree" if the host
        // filesystem also lacks one — which it should.
        // We at least assert it does NOT pick the tmp dir itself.
        if let Some(found) = walk_to_workspace_root(&nested) {
            assert_ne!(found, tmp.path());
        }
    }
}
