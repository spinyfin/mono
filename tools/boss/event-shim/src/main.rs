//! `boss-event` — a thin stdin-to-Unix-socket shim invoked by claude
//! hooks running inside a Boss-managed worker.
//!
//! This is the delivery channel for the Claude driver's rich-tier
//! **ProgressObservation** capability: the engine wires every Claude hook
//! event to this shim (see `driver::ClaudeDriver::progress_observation_wiring`),
//! the shim forwards each payload to the engine, and the engine decodes it
//! into a `WorkerEvent` (`driver::ClaudeDriver::normalize_progress_event`).
//! The shim itself is driver-agnostic transport — it splices run identity and
//! forwards bytes; it does not interpret the hook schema.
//!
//! Each claude hook is configured (via the engine's per-worker
//! settings-file template) to spawn this binary, with the hook
//! payload arriving on stdin. The shim reads stdin to EOF, opens the
//! engine's events socket at `$BOSS_EVENTS_SOCKET`, writes the
//! payload, and exits.
//!
//! ## Resilience
//!
//! Hooks fire on the worker's hot path and the engine is allowed to
//! restart out from under the worker. The shim survives an unreachable
//! engine without dropping events:
//!
//! 1. **Bounded retry on connect.** Up to a handful of attempts with
//!    exponential backoff, totaling ~10s of wall clock. The same
//!    schedule applies to the current event and to the head of a
//!    non-empty buffer drain, so an engine restart still gets the
//!    ride-through when older events are queued. After exhaustion, the
//!    event is appended to an on-disk buffer in the worker's workspace
//!    (`.boss/events-pending.jsonl`) and the shim exits zero — the
//!    worker keeps moving, and the engine sees the event next time
//!    it's reachable.
//! 2. **Reconnect on mid-send failure.** If a previously-good
//!    connection dies between connect and EOF (broken pipe), the shim
//!    reopens once and resends.
//! 3. **Drain on success.** Before sending the current event the shim
//!    opportunistically drains any buffered events from previous
//!    engine-down windows, oldest first. A concurrent drain holds the
//!    buffer lock; this invocation waits for that lock up to the
//!    remaining budget, then drains and sends, so a turn-ending Stop
//!    is not stranded until some later hook fires. A permanently
//!    rejected event is reported and discarded so it cannot block
//!    later events; a transient failure stops the drain and leaves its
//!    unsent suffix queued.
//! 4. **Bounded buffer.** The buffer is capped at the most recent
//!    [`MAX_BUFFERED_EVENTS`] events. A persistently-down engine can't
//!    cause the buffer to grow unbounded.
//! 5. **Total socket-work budget.** Drain, connect retries, writes, and
//!    the mid-send reconnect share one [`SHIM_TOTAL_BUDGET`] deadline
//!    stamped at the start of `run()`. Whichever socket stage is running
//!    when the budget runs out bails out to `buffer_or_lose` immediately
//!    rather than continuing. Local buffer appends retain their blocking
//!    lock so the final event is not lost.
//!
//! The shim is intentionally still small and synchronous: the
//! resilience layer is local file I/O plus retry, no async runtime.
//!
//! The engine derives the worker's lease via `LOCAL_PEERPID`
//! on its side, so the shim doesn't need to embed the lease id, only
//! the raw hook JSON.
//!
//! ## Exit codes and engine-side rejection
//!
//! Every refusal to deliver — a missing `BOSS_EVENTS_SOCKET`, empty
//! stdin, or a delivery that failed and could not even be buffered —
//! exits non-zero with a diagnostic on stderr naming what failed. The
//! one case that exits zero despite not (yet) reaching the engine is a
//! connect/write failure that *was* successfully queued to the on-disk
//! buffer for later delivery — see "Resilience" above; that is a real,
//! bounded mitigation, not a silent loss, and callers can distinguish it
//! from an outright failure by the "buffering event for later delivery"
//! message on stderr.
//!
//! A write succeeding is not the same as the engine accepting the
//! event: after the half-close, [`read_response`] waits briefly for a
//! [`REJECTION_PREFIX`] notice the engine writes back when it rejects a
//! connection (malformed payload, unresolved driver, ...), or a
//! [`RETRY_PREFIX`] notice when the failure was transient (a socket
//! hiccup, a busy database lookup). The rejection case exits non-zero
//! with the engine's own reason and is never buffered — resending a
//! payload the engine actively refused would just be refused again. The
//! retry case is treated exactly like an unreachable engine: the event is
//! queued to the on-disk buffer for later redelivery.

mod pending;

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use fs4::fs_std::FileExt;

/// Engine events socket path. Required.
const SOCKET_ENV: &str = "BOSS_EVENTS_SOCKET";
/// Pending-hook spool directory. Must match `boss_host_execution::worker::EVENT_SPOOL_ENV`.
const EVENT_SPOOL_ENV: &str = "BOSS_EVENT_SPOOL";
/// Run id to splice into payloads. Optional — splice falls back to
/// forwarding the original payload when unset.
const RUN_ID_ENV: &str = "BOSS_RUN_ID";
/// Absolute path to the worker's workspace. Optional — falls back to
/// the shim's `cwd`. Used to locate the per-workspace event buffer.
const WORKSPACE_ENV: &str = "BOSS_WORKSPACE";
/// Comma-separated milliseconds list overriding the connect-retry
/// backoff schedule. Tests use this to keep wall time bounded; production
/// leaves it unset and gets [`DEFAULT_RETRY_DELAYS_MS`].
const RETRY_DELAYS_ENV: &str = "BOSS_EVENT_RETRY_DELAYS_MS";

/// On-disk buffer path, relative to the workspace root.
const BUFFER_REL_PATH: &str = ".boss/events-pending.jsonl";

/// On-disk quarantine path, relative to the workspace root, that a
/// permanently rejected buffered event is appended to before it is
/// discarded from [`BUFFER_REL_PATH`]. Hook stderr on a non-blocking hook
/// is not surfaced to anyone by default, so without this a discard is
/// invisible; this keeps the payload and the engine's rejection reason
/// around for forensics without re-queuing it for redelivery.
const QUARANTINE_REL_PATH: &str = ".boss/events-rejected.jsonl";

/// Cap on buffered events. Past this the oldest events are dropped so
/// a long engine-down window can't grow the file without bound.
const MAX_BUFFERED_EVENTS: usize = 1000;

/// Default backoff schedule between connect attempts (in addition to
/// the initial attempt). Sums to ~10.2s of wall time across five
/// retries, matching the design budget below.
const DEFAULT_RETRY_DELAYS_MS: &[u64] = &[200, 500, 1500, 3000, 5000];

/// Upper bound on a single connected write / half-close so a peer that
/// accepts but never reads cannot pin the hook (and, on Grok, the TUI)
/// for the full write. The write timeout actually used is the smaller
/// of this and whatever remains of [`SHIM_TOTAL_BUDGET`] — see
/// `write_timeout_for`.
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Marker line the engine writes back on the socket when it rejects a
/// connection (malformed JSON, unrecognized hook shape, unresolved
/// driver, ...) rather than accepting it. A successful delivery writes
/// `BOSS-EVENT-RECORDED`; older engines still close with no bytes. Bytes
/// arriving with this prefix mean the write succeeded but the event was
/// never actually accepted.
///
/// Deliberately duplicated (not shared via a crate dependency) from
/// `tools/boss/engine/core/src/events_socket.rs`'s `REJECTION_PREFIX`:
/// this shim is intentionally a tiny, dependency-free binary with no
/// link to the engine crate, so the two copies of this literal are the
/// whole contract. Keep them byte-for-byte identical if either changes.
const REJECTION_PREFIX: &str = "BOSS-EVENT-REJECTED: ";

/// Marker line the engine writes back on the socket when it hit a
/// transient error (a socket I/O hiccup, or a busy/locked database lookup)
/// rather than a permanent, guaranteed-to-fail-again one. Unlike
/// [`REJECTION_PREFIX`], this means the identical payload could well
/// succeed on the very next attempt, so the shim treats it the same as an
/// unreachable engine: queue the payload back onto the on-disk buffer for
/// later redelivery rather than discarding it.
///
/// Deliberately duplicated (not shared via a crate dependency) from
/// `tools/boss/engine/core/src/events_socket.rs`'s `RETRY_PREFIX`, for the
/// same dependency-free-shim reason as [`REJECTION_PREFIX`]. Keep them
/// byte-for-byte identical if either changes.
const RETRY_PREFIX: &str = "BOSS-EVENT-RETRY: ";

/// Upper bound on how long the shim waits, after finishing its write and
/// half-closing, for the engine to write back a [`REJECTION_PREFIX`],
/// [`RETRY_PREFIX`], or recorded notice. Bounded by whatever remains of
/// [`SHIM_TOTAL_BUDGET`] the same way every other socket stage is.
const REJECTION_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// Total wall-clock budget for one shim invocation, covering the
/// buffer drain, the current event's connect-retry-and-write, and the
/// mid-send reconnect — everything `run()` does before it either
/// succeeds or falls back to `buffer_or_lose`. Stamped once at the top
/// of `run()` as a deadline and checked before every sleep, connect,
/// and send so the shim can never blow past it, no matter how many
/// buffered events or retries are pending.
///
/// Kept strictly below the Grok progress-forwarder hook timeout
/// (`FORWARDER_HOOK_TIMEOUT_SEC` = 15s in `driver/src/grok/hooks.rs`):
/// Grok blocks its TUI for the full duration of every command hook, so
/// a shim that could still be running when that timeout fires gets
/// killed before it reaches `buffer_or_lose` — dropping the current
/// event outright, or (if killed mid-drain) leaving already-delivered
/// buffered events on disk to be redelivered. 3s of headroom below the
/// hook timeout covers process teardown and scheduling jitter.
const SHIM_TOTAL_BUDGET: Duration = Duration::from_secs(12);

// Must match the engine events socket's ingress limit.
const MAX_HOOK_BYTES: usize = 256 * 1024;

/// Compile-time headroom that the default connect-retry schedule must
/// leave within [`SHIM_TOTAL_BUDGET`] for a final write attempt. The
/// actual write timeout remains the time left at runtime because a
/// preceding buffer drain may have consumed part of the budget.
const MIN_WRITE_BUDGET_MS: u64 = 250;

const fn sum_ms(delays: &[u64]) -> u64 {
    let mut total = 0u64;
    let mut i = 0;
    while i < delays.len() {
        total += delays[i];
        i += 1;
    }
    total
}

// Compiler-enforced version of the invariant the comments above
// describe in prose: if `DEFAULT_RETRY_DELAYS_MS` is widened without
// revisiting `SHIM_TOTAL_BUDGET`, the build fails instead of silently
// reintroducing a worst-case wall time that outlives the shim's budget.
const _: () = assert!(
    sum_ms(DEFAULT_RETRY_DELAYS_MS) + MIN_WRITE_BUDGET_MS <= SHIM_TOTAL_BUDGET.as_millis() as u64,
    "DEFAULT_RETRY_DELAYS_MS plus the minimum write budget must fit within SHIM_TOTAL_BUDGET"
);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("boss-event: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let deadline = Instant::now() + SHIM_TOTAL_BUDGET;

    let socket_path =
        env::var(SOCKET_ENV).map_err(|_| anyhow!("{SOCKET_ENV} not set; refusing to deliver hook event"))?;

    let mut payload = Vec::new();
    io::stdin()
        .read_to_end(&mut payload)
        .context("reading hook payload from stdin")?;

    if payload.is_empty() {
        return Err(anyhow!("hook payload on stdin was empty"));
    }

    // If `BOSS_RUN_ID` is set in the worker's env, splice it into the
    // hook JSON object so the engine can correlate this event to the
    // run without needing a working shell-pid lookup. Always splice a
    // stable `_boss_delivery_id` so a timed-out acknowledgement can be
    // retried without the engine dispatching the same payload twice.
    // On any failure (payload not a JSON object) we forward the original
    // bytes unchanged so the shim stays best-effort and never blocks
    // the worker.
    let payload_line = splice_shim_fields(&payload);

    if let Some(directory) = env::var_os(EVENT_SPOOL_ENV) {
        return pending::deliver(Path::new(&directory), &socket_path, &payload_line, deadline);
    }

    let buffer_path = resolve_buffer_path();
    let quarantine_path = resolve_quarantine_path();
    deliver_local(&socket_path, buffer_path, quarantine_path, payload_line, deadline)
}

fn deliver_local(
    socket_path: &str,
    buffer_path: Option<PathBuf>,
    quarantine_path: Option<PathBuf>,
    payload_line: Vec<u8>,
    deadline: Instant,
) -> Result<()> {
    if payload_line.len() > MAX_HOOK_BYTES {
        return reject_delivery(socket_path, "hook exceeds ingress size limit after truncation");
    }
    // Drain leftover buffered events FIRST (oldest first). The head
    // record retries connect under `deadline` so an engine restart still
    // rides through when the buffer is non-empty. A concurrent drain
    // holds the exclusive lock; we wait for it rather than treating
    // contention as a failure that would strand this hook. Draining
    // before the current event preserves FIFO ordering on the engine's
    // accept queue.
    if let Some(buf) = buffer_path.as_deref()
        && let Err(err) = drain_buffer(socket_path, buf, quarantine_path.as_deref(), deadline)
    {
        eprintln!("boss-event: drain of {} skipped: {err:#}", buf.display());
        return buffer_or_lose(Some(buf), &payload_line);
    }

    // Then send the current event, with bounded connect-retry and a
    // single mid-send reconnect on broken pipe. Both stages are bounded
    // by `deadline`, which was stamped before the drain above — so the
    // total across drain + this send can never exceed SHIM_TOTAL_BUDGET.
    match connect_with_retry(socket_path, deadline) {
        Ok(stream) => match send_to_stream(stream, &payload_line, deadline) {
            Ok(EngineResponse::None | EngineResponse::Recorded) => Ok(()),
            Ok(EngineResponse::Rejected(reason)) => reject_delivery(socket_path, &reason),
            Ok(EngineResponse::Retry(reason)) => {
                eprintln!(
                    "boss-event: engine hit a transient error over {socket_path}: {reason}; \
                     buffering event for later delivery",
                );
                buffer_or_lose(buffer_path.as_deref(), &payload_line)
            }
            Err(_first_err) => {
                // Mid-send failure: the engine may have bounced
                // between connect and write. Reopen once and resend,
                // unless the budget is already gone.
                if budget_exhausted(deadline) {
                    eprintln!(
                        "boss-event: events socket {socket_path} dropped mid-send and the shim's \
                         wall-clock budget is exhausted; buffering event for later delivery",
                    );
                    buffer_or_lose(buffer_path.as_deref(), &payload_line)
                } else {
                    match connect_once(socket_path, write_timeout_for(deadline))
                        .and_then(|s| send_to_stream(s, &payload_line, deadline))
                    {
                        Ok(EngineResponse::None | EngineResponse::Recorded) => Ok(()),
                        Ok(EngineResponse::Rejected(reason)) => reject_delivery(socket_path, &reason),
                        Ok(EngineResponse::Retry(reason)) => {
                            eprintln!(
                                "boss-event: engine hit a transient error over {socket_path} on \
                                 reconnect: {reason}; buffering event for later delivery",
                            );
                            buffer_or_lose(buffer_path.as_deref(), &payload_line)
                        }
                        Err(err) => {
                            eprintln!(
                                "boss-event: events socket {socket_path} dropped mid-send and \
                                 reconnect failed: {err:#}; buffering event for later delivery",
                            );
                            buffer_or_lose(buffer_path.as_deref(), &payload_line)
                        }
                    }
                }
            }
        },
        Err(err) => {
            eprintln!(
                "boss-event: events socket {socket_path} unreachable after retries: {err}; \
                 buffering event for later delivery",
            );
            buffer_or_lose(buffer_path.as_deref(), &payload_line)
        }
    }
}

/// The engine actively rejected the connection (malformed payload,
/// unresolved driver, ...) rather than failing to receive it at all.
/// This is never transient in the way an unreachable socket is, so
/// unlike the connect/write failure paths above, this does not buffer
/// for a later retry -- resending the same payload would just be
/// rejected again. Surfaces the engine's own reason on stderr and exits
/// non-zero so the caller can tell "configured and delivered" apart
/// from "configured, connected, but refused".
fn reject_delivery(socket_path: &str, reason: &str) -> Result<()> {
    eprintln!("boss-event: engine rejected the hook event over {socket_path}: {reason}");
    Err(anyhow!("engine rejected hook event: {reason}"))
}

/// Wall time remaining until `deadline` as of `now`, saturating at zero.
fn remaining_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

/// Wall time remaining until `deadline`, saturating at zero.
fn remaining(deadline: Instant) -> Duration {
    remaining_at(deadline, Instant::now())
}

/// Whether the shim's total wall-clock budget has already run out as of `now`.
fn budget_exhausted_at(deadline: Instant, now: Instant) -> bool {
    remaining_at(deadline, now).is_zero()
}

/// Whether the shim's total wall-clock budget has already run out.
fn budget_exhausted(deadline: Instant) -> bool {
    budget_exhausted_at(deadline, Instant::now())
}

/// The write timeout to use for a connect happening now: the smaller of
/// [`SOCKET_IO_TIMEOUT`] and whatever budget remains, so a stalled write
/// can never by itself push the shim past `deadline`.
fn write_timeout_for(deadline: Instant) -> Duration {
    remaining(deadline).min(SOCKET_IO_TIMEOUT)
}

/// Try to append `payload` to the on-disk buffer. If no buffer path
/// could be resolved (no workspace, cwd unwritable), surface a hard
/// error so claude logs the dropped event — better than a silent loss.
fn buffer_or_lose(buffer_path: Option<&Path>, payload: &[u8]) -> Result<()> {
    let Some(buffer_path) = buffer_path else {
        return Err(anyhow!(
            "no workspace buffer path available (set {WORKSPACE_ENV} or run from a writable cwd); event dropped"
        ));
    };
    append_to_buffer(buffer_path, payload).with_context(|| format!("buffering event to {}", buffer_path.display()))
}

/// Inject `_boss_run_id` and a stable `_boss_delivery_id` into a hook
/// JSON object payload. Missing env or a non-object payload leaves the
/// original bytes unchanged. An already-present delivery id (a buffered
/// retry of this same payload) is kept so the engine can drop duplicates.
fn splice_shim_fields(payload: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = match maybe_splice_run_id(payload) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => return bytes,
        },
        Err(_) => match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => return payload.to_vec(),
        },
    };
    let Some(object) = value.as_object_mut() else {
        return payload.to_vec();
    };
    let has_delivery_id = object
        .get("_boss_delivery_id")
        .and_then(|value| value.as_str())
        .is_some_and(|id| !id.is_empty());
    if !has_delivery_id {
        object.insert(
            "_boss_delivery_id".to_owned(),
            serde_json::Value::String(generate_delivery_id()),
        );
    }
    let encoded = serde_json::to_vec(&value).unwrap_or_else(|_| payload.to_vec());
    if encoded.len() <= MAX_HOOK_BYTES {
        return encoded;
    }
    let object = value.as_object_mut().expect("hook object");
    pending::bound_fields(object);
    object.insert("_boss_payload_truncated".into(), true.into());
    serde_json::to_vec(&value).unwrap_or(encoded)
}

fn generate_delivery_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}-{n}", std::process::id())
}

/// Inject `_boss_run_id` into a hook JSON object payload when the env
/// is present and the payload parses as a JSON object. The returned
/// bytes are compact JSON (no embedded newlines), so the result is safe
/// to write as a single line of `.jsonl`.
fn maybe_splice_run_id(payload: &[u8]) -> Result<Vec<u8>> {
    let run_id = env::var(RUN_ID_ENV).context("BOSS_RUN_ID not set")?;
    if run_id.is_empty() {
        return Err(anyhow!("BOSS_RUN_ID is empty"));
    }
    let mut value: serde_json::Value = serde_json::from_slice(payload).context("hook payload was not JSON")?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("hook payload was not a JSON object"))?;
    object.insert("_boss_run_id".to_owned(), serde_json::Value::String(run_id));
    Ok(serde_json::to_vec(&value)?)
}

/// Locate the per-workspace buffer file. `BOSS_WORKSPACE`, if set,
/// wins; otherwise we fall back to the shim's cwd, which is normally
/// the worker's workspace (claude inherits cwd from the spawned pane).
/// Returns `None` only if neither env nor cwd is available (extremely
/// rare — the process always has a cwd).
fn resolve_buffer_path() -> Option<PathBuf> {
    let root = match env::var(WORKSPACE_ENV) {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => env::current_dir().ok()?,
    };
    Some(root.join(BUFFER_REL_PATH))
}

/// Locate the per-workspace quarantine file, by the same root resolution
/// as [`resolve_buffer_path`].
fn resolve_quarantine_path() -> Option<PathBuf> {
    let root = match env::var(WORKSPACE_ENV) {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => env::current_dir().ok()?,
    };
    Some(root.join(QUARANTINE_REL_PATH))
}

/// Parse `BOSS_EVENT_RETRY_DELAYS_MS` if set, else return the default
/// schedule. Malformed values fall back to the default so a typo in env
/// can't accidentally disable retries in production.
fn retry_delays() -> Vec<Duration> {
    if let Ok(raw) = env::var(RETRY_DELAYS_ENV)
        && !raw.is_empty()
    {
        let parsed: Result<Vec<u64>, _> = raw.split(',').map(|s| s.trim().parse::<u64>()).collect();
        if let Ok(values) = parsed {
            return values.into_iter().map(Duration::from_millis).collect();
        }
    }
    DEFAULT_RETRY_DELAYS_MS
        .iter()
        .map(|ms| Duration::from_millis(*ms))
        .collect()
}

/// One connect attempt, no retry. Applies `write_timeout` so a
/// subsequent `write_all` cannot block unbounded if the peer stalls.
/// `send_to_stream` sets a short read timeout after half-closing so it
/// can observe an engine rejection notice.
fn connect_once(path: &str, write_timeout: Duration) -> Result<UnixStream> {
    let stream = UnixStream::connect(path).with_context(|| format!("connecting to events socket at {path}"))?;
    // Best-effort: a platform that rejects SO_SNDTIMEO still delivers
    // events; the shim's own wall-clock budget remains the outer ceiling.
    let _ = stream.set_write_timeout(Some(write_timeout));
    Ok(stream)
}

/// Bounded retry loop around `connect_once`. Sleeps between attempts
/// per [`retry_delays`], never past `deadline`. Returns an error as
/// soon as the budget runs out or every attempt fails, whichever comes
/// first.
fn connect_with_retry(path: &str, deadline: Instant) -> Result<UnixStream> {
    connect_with_retry_using(path, deadline, thread::sleep)
}

/// [`connect_with_retry`] with an explicit sleeper so tests can prove an
/// expired budget never enters the retry schedule.
fn connect_with_retry_using(path: &str, deadline: Instant, sleep: impl Fn(Duration)) -> Result<UnixStream> {
    if budget_exhausted(deadline) {
        return Err(anyhow!("shim wall-clock budget exhausted before first connect attempt"));
    }
    if let Ok(stream) = connect_once(path, write_timeout_for(deadline)) {
        return Ok(stream);
    }
    let delays = retry_delays();
    let mut last_err: Option<anyhow::Error> = None;
    for delay in delays {
        let time_left = remaining(deadline);
        if time_left.is_zero() {
            return Err(last_err.unwrap_or_else(|| anyhow!("shim wall-clock budget exhausted during connect retries")));
        }
        sleep(delay.min(time_left));
        if budget_exhausted(deadline) {
            return Err(last_err.unwrap_or_else(|| anyhow!("shim wall-clock budget exhausted during connect retries")));
        }
        match connect_once(path, write_timeout_for(deadline)) {
            Ok(stream) => return Ok(stream),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("connect retries exhausted with no error")))
}

/// Write payload to a connected stream, half-close, and return. The
/// engine reads to EOF, so the half-close is the message terminator.
///
/// Relies on the write timeout set in [`connect_once`]: without it, a
/// peer that accepts the connection but never drains the socket can
/// block `write_all` until the outer hook runner kills the process
/// (up to 600s for an unscoped Grok `Stop` hook — which freezes the TUI).
///
/// After the half-close, waits briefly for a [`REJECTION_PREFIX`] or
/// [`RETRY_PREFIX`] notice (see [`read_response`]) and returns the
/// decoded [`EngineResponse`]. A clean EOF is successful delivery; an
/// expired response window requests retry because a delayed decode may
/// still fail after the shim has stopped listening.
fn send_to_stream(mut stream: UnixStream, payload: &[u8], deadline: Instant) -> Result<EngineResponse> {
    stream
        .write_all(payload)
        .context("writing hook payload to events socket")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutting down write half of events socket")?;
    read_response(&mut stream, deadline)
}

/// The engine's write-back on the events socket after a connection closes:
/// a permanent rejection, a transient-error retry request, or silence
/// (a successful delivery confirmed by a clean EOF).
enum EngineResponse {
    Rejected(String),
    Retry(String),
    None,
    Recorded,
}

/// Wait, bounded by [`REJECTION_READ_TIMEOUT`] and whatever remains of
/// `deadline`, for the engine to write a [`REJECTION_PREFIX`],
/// [`RETRY_PREFIX`], or recorded notice back on `stream` before it closes.
/// Returns [`EngineResponse::None`] on a clean EOF. A timed-out read or
/// exhausted budget requests retry; the engine may still be decoding.
/// Redelivery is idempotent because [`splice_shim_fields`] stamps a stable
/// `_boss_delivery_id` that the engine drops on a later drain.
fn read_response(stream: &mut UnixStream, deadline: Instant) -> Result<EngineResponse> {
    let timeout = remaining(deadline).min(REJECTION_READ_TIMEOUT);
    if timeout.is_zero() {
        return Ok(EngineResponse::Retry(
            "response budget exhausted before acknowledgement".into(),
        ));
    }
    // Best-effort: a platform that rejects SO_RCVTIMEO just risks a
    // blocking read, bounded in practice by the engine always closing
    // its write side once it's done (success, rejection, or retry).
    let _ = stream.set_read_timeout(Some(timeout));
    let mut buf = Vec::new();
    let read_error = stream.read_to_end(&mut buf).err();
    let text = String::from_utf8_lossy(&buf);
    if text.trim() == "BOSS-EVENT-RECORDED" {
        return Ok(EngineResponse::Recorded);
    }
    if let Some(reason) = text.strip_prefix(REJECTION_PREFIX) {
        // A peer can deliver its rejection and then reset the
        // connection. The bytes are authoritative, so do not let the
        // trailing read error reclassify this as a transient failure.
        return Ok(EngineResponse::Rejected(reason.trim().to_owned()));
    }
    if let Some(reason) = text.strip_prefix(RETRY_PREFIX) {
        return Ok(EngineResponse::Retry(reason.trim().to_owned()));
    }
    if let Some(err) = read_error {
        if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) {
            return Ok(EngineResponse::Retry(
                "engine response timed out before acknowledgement".into(),
            ));
        }
        return Err(err).context("reading engine response from events socket");
    }
    Ok(EngineResponse::None)
}

enum BufferedSendError {
    Rejected(String),
    Transient(anyhow::Error),
}

/// Send one buffered event in its own connection. Used by drain. The
/// head record retries connect under `deadline` so an engine restart
/// still rides through when the buffer is non-empty. A later record
/// that cannot connect before the budget runs out stops the drain and
/// keeps FIFO order. Fails fast once `deadline` has passed, without
/// attempting a connect.
fn send_one(socket_path: &str, payload: &[u8], deadline: Instant) -> std::result::Result<(), BufferedSendError> {
    if payload.len() > MAX_HOOK_BYTES {
        return Err(BufferedSendError::Rejected("hook exceeds ingress size limit".into()));
    }
    if budget_exhausted(deadline) {
        return Err(BufferedSendError::Transient(anyhow!(
            "shim wall-clock budget exhausted before send"
        )));
    }
    let stream = connect_with_retry(socket_path, deadline).map_err(BufferedSendError::Transient)?;
    match send_to_stream(stream, payload, deadline).map_err(BufferedSendError::Transient)? {
        EngineResponse::None | EngineResponse::Recorded => Ok(()),
        EngineResponse::Rejected(reason) => Err(BufferedSendError::Rejected(reason)),
        EngineResponse::Retry(reason) => Err(BufferedSendError::Transient(anyhow!(
            "engine reported a transient error: {reason}"
        ))),
    }
}

/// Append `payload` as a new line to the workspace's event buffer.
/// Creates the parent directory if needed, takes an advisory exclusive
/// blocking advisory exclusive lock for the duration of the write so a
/// concurrent shim invocation can't interleave bytes mid-line. This
/// final persistence step deliberately waits for the lock rather than
/// dropping an event, and rotates the file when it grows past
/// [`MAX_BUFFERED_EVENTS`] lines.
fn append_to_buffer(buffer_path: &Path, payload: &[u8]) -> Result<()> {
    if let Some(parent) = buffer_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {} ", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(buffer_path)
        .with_context(|| format!("opening {}", buffer_path.display()))?;
    FileExt::lock_exclusive(&file).context("locking event buffer for append")?;
    let _guard = LockGuard(&file);

    let mut writer = &file;
    writer.write_all(payload).context("writing payload to event buffer")?;
    writer.write_all(b"\n").context("writing terminator to event buffer")?;
    writer.flush().context("flushing event buffer")?;

    // Cheap line-count check. Hook payloads are small (< a few KB), and
    // claude fires hooks sequentially per session, so re-reading the
    // file here is fine even on the hot path.
    let count = count_lines(&file)?;
    if count > MAX_BUFFERED_EVENTS {
        trim_to_last(&file, MAX_BUFFERED_EVENTS)?;
    }
    Ok(())
}

/// Read the buffer line by line and try to deliver each event. A
/// permanently rejected event is reported on stderr, appended to
/// `quarantine_path` (best-effort — a quarantine-write failure is only
/// logged, it never re-queues the event or aborts the drain) for later
/// forensics, and discarded from the buffer; a transient failure —
/// including the shim's wall-clock budget running out — stops the drain
/// and retains that event plus its FIFO suffix. Truncates the buffer file
/// to zero length when every event was delivered or discarded; leaves it
/// untouched when nothing changed (nothing to send, or the very first
/// record hit a transient failure), so a kill between truncate and
/// rewrite can never lose more than what this drain actually consumed.
/// No-op when the buffer is absent. If another shim is already draining,
/// waits for the exclusive lock up to `deadline` so the current hook is
/// drained or sent by this invocation instead of stranded until later.
fn drain_buffer(
    socket_path: &str,
    buffer_path: &Path,
    quarantine_path: Option<&Path>,
    deadline: Instant,
) -> Result<()> {
    if !buffer_path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(buffer_path)
        .with_context(|| format!("opening {} for drain", buffer_path.display()))?;
    lock_exclusive_until(&file, deadline).context("locking event buffer for drain")?;
    let _guard = LockGuard(&file);

    let lines = read_lines(&file)?;
    let mut drain_err: Option<anyhow::Error> = None;
    let mut unsent_start = lines.len();
    let mut discarded = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if budget_exhausted(deadline) {
            drain_err = Some(anyhow!("shim wall-clock budget exhausted during buffer drain"));
            unsent_start = index;
            break;
        }
        match send_one(socket_path, line, deadline) {
            Ok(()) => {}
            Err(BufferedSendError::Rejected(reason)) => {
                eprintln!(
                    "boss-event: discarding permanently rejected buffered hook event from {}: {reason}",
                    buffer_path.display()
                );
                if let Some(quarantine_path) = quarantine_path
                    && let Err(err) = append_to_quarantine(quarantine_path, line, &reason)
                {
                    eprintln!(
                        "boss-event: failed to quarantine rejected event to {}: {err:#}",
                        quarantine_path.display()
                    );
                }
                discarded += 1;
            }
            Err(BufferedSendError::Transient(err)) => {
                drain_err = Some(err);
                unsent_start = index;
                break;
            }
        }
    }

    if unsent_start > 0 || discarded > 0 {
        rewrite_lines(&file, &lines[unsent_start..])?;
    }

    if let Some(err) = drain_err {
        return Err(err);
    }
    Ok(())
}

/// Wait for an exclusive advisory lock on `file` until `deadline`.
/// Contention is a wait, not a failure: the lock holder is the in-flight
/// drain that must finish before this invocation can send in FIFO order.
fn lock_exclusive_until(file: &File, deadline: Instant) -> Result<()> {
    loop {
        match FileExt::try_lock_exclusive(file) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                let time_left = remaining(deadline);
                if time_left.is_zero() {
                    return Err(anyhow!("timed out waiting for event buffer drain lock"));
                }
                thread::sleep(Duration::from_millis(10).min(time_left));
            }
            Err(err) => return Err(err).context("locking event buffer for drain"),
        }
    }
}

/// Append a rejected buffered event, plus the engine's rejection reason,
/// as one JSON line to `quarantine_path`. Reuses [`append_to_buffer`]'s
/// create-parent/lock/append shape and its [`MAX_BUFFERED_EVENTS`] trim, so
/// a persistently-failing engine can't grow the quarantine file unbounded
/// either.
fn append_to_quarantine(quarantine_path: &Path, payload: &[u8], reason: &str) -> Result<()> {
    let record = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(event) => serde_json::json!({ "rejected_reason": reason, "event": event }),
        Err(_) => serde_json::json!({
            "rejected_reason": reason,
            "event_raw": String::from_utf8_lossy(payload),
        }),
    };
    let line = serde_json::to_vec(&record).context("encoding quarantined event")?;
    append_to_buffer(quarantine_path, &line)
}

/// Count the lines (newline-terminated records) in the buffer file.
fn count_lines(file: &File) -> Result<usize> {
    let mut f = file;
    f.seek(SeekFrom::Start(0))
        .context("seeking buffer file to count lines")?;
    let reader = BufReader::new(f);
    let mut n = 0usize;
    for line in reader.split(b'\n') {
        let line = line.context("reading buffer line")?;
        if !line.is_empty() {
            n += 1;
        }
    }
    Ok(n)
}

/// Read every non-empty newline-terminated record from the buffer.
fn read_lines(file: &File) -> Result<Vec<Vec<u8>>> {
    let mut f = file;
    f.seek(SeekFrom::Start(0)).context("seeking buffer file to read")?;
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for line in reader.split(b'\n') {
        let line = line.context("reading buffer line")?;
        if !line.is_empty() {
            out.push(line);
        }
    }
    Ok(out)
}

/// Trim the buffer to its last `keep` lines (in place).
fn trim_to_last(file: &File, keep: usize) -> Result<()> {
    let lines = read_lines(file)?;
    if lines.len() <= keep {
        return Ok(());
    }
    let tail = &lines[lines.len() - keep..];
    rewrite_lines(file, tail)
}

/// Replace the buffer contents with `lines` in order. Truncates first
/// so a shorter payload doesn't leave a tail of stale bytes behind.
fn rewrite_lines(file: &File, lines: &[Vec<u8>]) -> Result<()> {
    let mut f = file;
    f.set_len(0).context("truncating event buffer")?;
    f.seek(SeekFrom::Start(0)).context("seeking event buffer to start")?;
    for line in lines {
        f.write_all(line).context("rewriting buffer line")?;
        f.write_all(b"\n").context("rewriting buffer terminator")?;
    }
    f.flush().context("flushing rewritten buffer")?;
    Ok(())
}

/// Drop-guard that releases the advisory lock at scope exit.
struct LockGuard<'a>(&'a File);
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_timeout_requires_retry_but_clean_eof_is_delivered() {
        let (mut client, server) = UnixStream::pair().unwrap();
        assert!(matches!(
            read_response(&mut client, Instant::now() + Duration::from_millis(30)).unwrap(),
            EngineResponse::Retry(_)
        ));
        drop(server);
        assert!(matches!(
            read_response(&mut client, Instant::now() + Duration::from_secs(1)).unwrap(),
            EngineResponse::None
        ));
        assert!(matches!(
            read_response(&mut client, Instant::now()).unwrap(),
            EngineResponse::Retry(_)
        ));
    }

    /// Global lock for tests that mutate the process env. Without this,
    /// parallel cargo test threads racing on the same env key produce
    /// occasional spurious failures (one test sees another's value).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior = env::var(key).ok();
        // SAFETY: ENV_LOCK serializes env mutation across tests. Each
        // test uses the env synchronously inside `f`.
        unsafe {
            match value {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
        let out = f();
        unsafe {
            match prior {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
        out
    }

    #[test]
    fn splice_inserts_run_id_into_object_payload() {
        with_env(RUN_ID_ENV, Some("run-xyz"), || {
            let payload = br#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#;
            let result = maybe_splice_run_id(payload).unwrap();
            let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
            assert_eq!(parsed["_boss_run_id"], "run-xyz");
            // Original fields must survive.
            assert_eq!(parsed["hook_event_name"], "PreToolUse");
            assert_eq!(parsed["tool_name"], "Bash");
        });
    }

    #[test]
    fn splice_errors_when_env_missing_so_caller_falls_back() {
        with_env(RUN_ID_ENV, None, || {
            let payload = br#"{"hook_event_name":"PreToolUse"}"#;
            assert!(maybe_splice_run_id(payload).is_err());
        });
    }

    #[test]
    fn splice_errors_when_env_empty() {
        with_env(RUN_ID_ENV, Some(""), || {
            let payload = br#"{"hook_event_name":"PreToolUse"}"#;
            assert!(maybe_splice_run_id(payload).is_err());
        });
    }

    #[test]
    fn splice_errors_when_payload_not_a_json_object() {
        with_env(RUN_ID_ENV, Some("run-xyz"), || {
            assert!(maybe_splice_run_id(b"not json at all").is_err());
            assert!(maybe_splice_run_id(b"\"a string\"").is_err());
            assert!(maybe_splice_run_id(b"[1,2,3]").is_err());
        });
    }

    #[test]
    fn oversized_hook_retains_lifecycle_and_delivery_fields() {
        with_env(RUN_ID_ENV, Some("run-large"), || {
            let payload = serde_json::json!({
                "hook_event_name": "PostToolUse", "session_id": "parent",
                "tool_name": "Write", "tool_input": {"content": "x".repeat(300 * 1024)},
                "stop_hook_active": false, "_boss_delivery_id": "stable-delivery",
            });
            let bounded = splice_shim_fields(payload.to_string().as_bytes());
            assert!(bounded.len() <= MAX_HOOK_BYTES);
            let parsed: serde_json::Value = serde_json::from_slice(&bounded).unwrap();
            assert_eq!(parsed["_boss_payload_truncated"], true);
            assert_eq!(parsed["tool_input"]["_boss_truncated"], true);
            assert_eq!(parsed["session_id"], "parent");
            assert_eq!(parsed["hook_event_name"], "PostToolUse");
            assert_eq!(parsed["stop_hook_active"], false);
            assert_eq!(parsed["_boss_run_id"], "run-large");
            assert_eq!(parsed["_boss_delivery_id"], "stable-delivery");
        });
    }

    #[test]
    fn splice_shim_fields_stamps_a_delivery_id_and_keeps_an_existing_one() {
        with_env(RUN_ID_ENV, Some("run-xyz"), || {
            let first = splice_shim_fields(br#"{"hook_event_name":"Stop"}"#);
            let parsed: serde_json::Value = serde_json::from_slice(&first).unwrap();
            assert_eq!(parsed["_boss_run_id"], "run-xyz");
            let delivery_id = parsed["_boss_delivery_id"].as_str().expect("delivery id");
            assert!(!delivery_id.is_empty());
            let again = splice_shim_fields(&first);
            let parsed_again: serde_json::Value = serde_json::from_slice(&again).unwrap();
            assert_eq!(parsed_again["_boss_delivery_id"], delivery_id);
        });
    }

    #[test]
    fn retry_delays_parses_env_override() {
        with_env(RETRY_DELAYS_ENV, Some("10,20,30"), || {
            let delays = retry_delays();
            assert_eq!(
                delays,
                vec![
                    Duration::from_millis(10),
                    Duration::from_millis(20),
                    Duration::from_millis(30),
                ],
            );
        });
    }

    #[test]
    fn retry_delays_falls_back_to_default_on_unset() {
        with_env(RETRY_DELAYS_ENV, None, || {
            let delays = retry_delays();
            assert_eq!(delays.len(), DEFAULT_RETRY_DELAYS_MS.len());
        });
    }

    #[test]
    fn retry_delays_falls_back_to_default_on_garbage() {
        with_env(RETRY_DELAYS_ENV, Some("garbage"), || {
            let delays = retry_delays();
            assert_eq!(delays.len(), DEFAULT_RETRY_DELAYS_MS.len());
        });
    }

    #[test]
    fn remaining_at_is_zero_once_now_reaches_the_deadline() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(1);
        assert!(remaining_at(t0, later).is_zero());
        assert!(budget_exhausted_at(t0, later));
        assert_eq!(remaining_at(later, t0), Duration::from_secs(1));
        assert!(!budget_exhausted_at(later, t0));
    }

    #[test]
    fn connect_with_retry_skips_attempts_after_budget_expires() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("never-bound.sock");
        let result = connect_with_retry_using(socket.to_str().unwrap(), Instant::now(), |_| {
            panic!("an expired budget must skip the retry schedule")
        });

        assert!(result.is_err(), "an expired budget must reject the connect");
    }

    #[test]
    fn append_then_read_roundtrips_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        append_to_buffer(&buf, b"{\"a\":1}").unwrap();
        append_to_buffer(&buf, b"{\"b\":2}").unwrap();
        append_to_buffer(&buf, b"{\"c\":3}").unwrap();
        let file = File::open(&buf).unwrap();
        let lines = read_lines(&file).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], b"{\"a\":1}");
        assert_eq!(lines[2], b"{\"c\":3}");
    }

    #[test]
    fn rewrite_lines_truncates_and_replaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        std::fs::write(&buf, b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n{\"d\":4}\n").unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&buf).unwrap();
        // Simulate a successful drain of the first two events: rewrite
        // with just the tail.
        let lines: Vec<Vec<u8>> = vec![b"{\"c\":3}".to_vec(), b"{\"d\":4}".to_vec()];
        rewrite_lines(&file, &lines).unwrap();

        let contents = std::fs::read_to_string(&buf).unwrap();
        assert_eq!(contents, "{\"c\":3}\n{\"d\":4}\n");
    }

    /// Drain against an unreachable socket leaves the buffer file
    /// untouched: no events should be lost if the engine never came up.
    #[test]
    fn drain_against_unreachable_socket_preserves_buffer() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        let original = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        std::fs::write(&buf, original).unwrap();

        let socket = dir.path().join("never-bound.sock");
        let deadline = Instant::now() + Duration::from_secs(1);
        let mtime_before = std::fs::metadata(&buf).unwrap().modified().unwrap();
        let result = drain_buffer(socket.to_str().unwrap(), &buf, None, deadline);
        assert!(result.is_err(), "drain must surface connect failure");

        let contents = std::fs::read(&buf).unwrap();
        assert_eq!(contents, original);
        // Nothing was sent or discarded, so the file must not even have
        // been rewritten (truncated + re-written byte-identically) —
        // proof there is no kill window here that could lose the buffer.
        assert_eq!(
            std::fs::metadata(&buf).unwrap().modified().unwrap(),
            mtime_before,
            "a drain that sends nothing must not touch the buffer file at all"
        );
    }

    /// A permanent rejection must not poison the FIFO queue: discard the
    /// rejected record and continue draining later records on fresh
    /// connections. The rejected record must also land in the quarantine
    /// file alongside the engine's rejection reason, so the discard is
    /// recoverable instead of vanishing into hook stderr.
    #[test]
    fn drain_discards_rejected_event_and_delivers_later_events() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        std::fs::write(&buf, b"bad-event\ngood-event\n").unwrap();
        let quarantine = dir.path().join(".boss/events-rejected.jsonl");
        let socket = dir.path().join("events.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let server = std::thread::spawn(move || {
            let (mut rejected, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            rejected.read_to_end(&mut payload).unwrap();
            assert_eq!(payload, b"bad-event");
            rejected.write_all(b"BOSS-EVENT-REJECTED: malformed event\n").unwrap();

            let (mut accepted, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            accepted.read_to_end(&mut payload).unwrap();
            payload
        });

        drain_buffer(
            socket.to_str().unwrap(),
            &buf,
            Some(&quarantine),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(server.join().unwrap(), b"good-event");
        assert_eq!(std::fs::read(&buf).unwrap(), b"");

        let quarantined = std::fs::read_to_string(&quarantine).unwrap();
        let record: serde_json::Value = serde_json::from_str(quarantined.trim()).unwrap();
        assert_eq!(record["rejected_reason"], "malformed event");
        assert_eq!(record["event_raw"], "bad-event");
    }

    /// A transient [`RETRY_PREFIX`] response must be treated exactly like
    /// an unreachable engine — not like a rejection: the drain stops and
    /// retains the record plus its FIFO suffix, rather than discarding it
    /// as if the engine had accepted (or permanently refused) it. This is
    /// the shim-side half of the fix for the class of bug where a
    /// transient engine error (e.g. a locked sqlite during a restart)
    /// silently dropped the buffered event.
    #[test]
    fn current_event_queues_behind_retrying_buffer() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        std::fs::write(&buf, b"transient-event\nlater-event\n").unwrap();
        let socket = dir.path().join("events.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            assert_eq!(payload, b"transient-event");
            stream.write_all(b"BOSS-EVENT-RETRY: database is locked\n").unwrap();
            // No second accept: the drain must stop after the retry
            // notice rather than moving on to `later-event`.
        });

        let result = deliver_local(
            socket.to_str().unwrap(),
            Some(buf.clone()),
            None,
            b"current-event".to_vec(),
            Instant::now() + Duration::from_secs(2),
        );
        result.unwrap();
        server.join().unwrap();

        assert_eq!(
            std::fs::read(&buf).unwrap(),
            b"transient-event\nlater-event\ncurrent-event\n",
            "the current event must queue behind the retried record and its FIFO suffix"
        );
    }

    /// Shim A holds the drain lock while shim B arrives with a Stop. B
    /// waits for the lock, then drains and sends; Stop reaches the
    /// socket without a third invocation.
    #[test]
    fn concurrent_stop_is_delivered_without_a_third_invocation() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        std::fs::write(&buf, b"earlier-event\n").unwrap();
        let socket = dir.path().join("events.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut conn, _) = listener.accept().unwrap();
                let mut payload = Vec::new();
                conn.read_to_end(&mut payload).unwrap();
                tx.send(payload).unwrap();
            }
        });

        let file = OpenOptions::new().read(true).write(true).open(&buf).unwrap();
        FileExt::lock_exclusive(&file).unwrap();
        let socket_owned = socket.to_str().unwrap().to_owned();
        let buf_for_b = buf.clone();
        let b = std::thread::spawn(move || {
            deliver_local(
                &socket_owned,
                Some(buf_for_b),
                None,
                b"stop-event".to_vec(),
                Instant::now() + Duration::from_secs(2),
            )
        });
        std::thread::sleep(Duration::from_millis(80));
        FileExt::unlock(&file).unwrap();
        drop(file);
        b.join().unwrap().unwrap();
        let first = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("earlier event must be drained");
        let second = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("Stop must be sent by the waiting shim");
        server.join().unwrap();
        assert_eq!(first, b"earlier-event");
        assert_eq!(second, b"stop-event");
        assert!(std::fs::read(&buf).unwrap().is_empty());
    }

    /// A non-empty buffer must still ride through an engine restart: the
    /// drain retries connect, then the buffered and current events are
    /// delivered in order once the socket appears.
    #[test]
    fn drain_retries_connect_when_engine_returns_and_keeps_fifo() {
        with_env(RETRY_DELAYS_ENV, Some("30,50,80"), || {
            let dir = tempfile::TempDir::new().unwrap();
            let buf = dir.path().join(".boss/events-pending.jsonl");
            std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
            std::fs::write(&buf, b"buffered-event\n").unwrap();
            let socket = dir.path().join("events.sock");
            let socket_for_server = socket.clone();
            let server = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                let listener = std::os::unix::net::UnixListener::bind(&socket_for_server).unwrap();
                let mut received = Vec::new();
                for _ in 0..2 {
                    let (mut conn, _) = listener.accept().unwrap();
                    let mut payload = Vec::new();
                    conn.read_to_end(&mut payload).unwrap();
                    received.push(payload);
                }
                received
            });
            deliver_local(
                socket.to_str().unwrap(),
                Some(buf.clone()),
                None,
                b"current-event".to_vec(),
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(
                server.join().unwrap(),
                vec![b"buffered-event".to_vec(), b"current-event".to_vec()]
            );
            assert!(std::fs::read(&buf).unwrap().is_empty());
        });
    }

    #[test]
    fn drain_stops_and_retains_record_on_retry_notice() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join("pending.jsonl");
        std::fs::write(&buf, b"transient-event\nlater-event\n").unwrap();
        let socket = dir.path().join("events.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).unwrap();
            assert_eq!(payload, b"transient-event");
            stream.write_all(b"BOSS-EVENT-RETRY: database is locked\n").unwrap();
        });
        assert!(
            drain_buffer(
                socket.to_str().unwrap(),
                &buf,
                None,
                Instant::now() + Duration::from_secs(2)
            )
            .is_err()
        );
        server.join().unwrap();
        assert_eq!(std::fs::read(&buf).unwrap(), b"transient-event\nlater-event\n");
    }

    #[test]
    fn drain_with_expired_budget_preserves_buffer() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        let original = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        std::fs::write(&buf, original).unwrap();

        let socket = dir.path().join("never-bound.sock");
        let deadline = Instant::now();
        assert!(
            budget_exhausted_at(deadline, Instant::now()),
            "the fixture deadline must already be past so drain never reaches connect"
        );
        let result = drain_buffer(socket.to_str().unwrap(), &buf, None, deadline);

        assert!(result.is_err(), "an expired budget must stop the drain");
        assert_eq!(std::fs::read(&buf).unwrap(), original);
    }

    /// Pins the invariant that `connect_once` applies a write timeout
    /// to the returned stream — including on the connect path
    /// `drain_buffer`/`send_one` uses, which is easy to miss since it
    /// goes through `connect_once` too rather than a separate path.
    #[test]
    fn connect_once_applies_write_timeout() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let stream = connect_once(sock_path.to_str().unwrap(), SOCKET_IO_TIMEOUT).unwrap();
        assert_eq!(stream.write_timeout().unwrap(), Some(SOCKET_IO_TIMEOUT));
    }

    /// A peer that accepts the connection but never reads must not be
    /// able to block `send_to_stream` past roughly the write timeout.
    /// Uses a payload larger than the kernel's socket send buffer
    /// (~1 MiB) — realistic for a PostToolUse hook payload carrying a
    /// large `tool_response`, and the only size where the write
    /// actually blocks instead of returning immediately into the
    /// kernel buffer.
    #[test]
    fn send_to_stream_times_out_on_stalled_peer() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let stream = connect_once(sock_path.to_str().unwrap(), SOCKET_IO_TIMEOUT).unwrap();
        let (_accepted, _addr) = listener.accept().unwrap();

        let payload = vec![0u8; 8 * 1024 * 1024];
        let start = Instant::now();
        let result = send_to_stream(stream, &payload, Instant::now() + Duration::from_secs(30));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "write to a stalled peer must time out, not succeed");
        // write_all can issue more than one underlying write() syscall
        // once the kernel send buffer is full (e.g. one partial write
        // that succeeds instantly, then a second that blocks and times
        // out), so allow a couple of SOCKET_IO_TIMEOUT-length blocks
        // rather than exactly one. What this pins down is that the
        // write is bounded at all, not left to hang for the ~600s an
        // unscoped hook runner would otherwise allow.
        assert!(
            elapsed < SOCKET_IO_TIMEOUT * 3,
            "write timeout not honored: took {elapsed:?}, expected roughly {SOCKET_IO_TIMEOUT:?}",
        );
    }

    #[test]
    fn append_trims_to_cap_when_exceeded() {
        // Lower the cap implicitly by writing > MAX events. We use a
        // small temp dir so the test runs fast; this exercises the
        // line-count + trim path on every append over the cap.
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        for i in 0..(MAX_BUFFERED_EVENTS + 5) {
            let line = format!("{{\"n\":{i}}}");
            append_to_buffer(&buf, line.as_bytes()).unwrap();
        }
        let file = File::open(&buf).unwrap();
        let lines = read_lines(&file).unwrap();
        assert_eq!(lines.len(), MAX_BUFFERED_EVENTS);
        // Oldest events were dropped: the first surviving line is the
        // (n+5)th original event, i.e. n == 5.
        assert_eq!(lines[0], format!("{{\"n\":{}}}", 5).as_bytes());
        assert_eq!(
            lines.last().unwrap(),
            format!("{{\"n\":{}}}", MAX_BUFFERED_EVENTS + 4).as_bytes(),
        );
    }
}
