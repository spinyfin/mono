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
//!    exponential backoff, totaling ~10s of wall clock. After
//!    exhaustion, the event is appended to an on-disk buffer in the
//!    worker's workspace (`.boss/events-pending.jsonl`) and the shim
//!    exits zero — the worker keeps moving, and the engine sees the
//!    event next time it's reachable.
//! 2. **Reconnect on mid-send failure.** If a previously-good
//!    connection dies between connect and EOF (broken pipe), the shim
//!    reopens once and resends.
//! 3. **Drain on success.** Before sending the current event the shim
//!    opportunistically drains any buffered events from previous
//!    engine-down windows, oldest first. Drain stops on the first
//!    failure; unsent events stay queued.
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

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use fs4::fs_std::FileExt;

/// Engine events socket path. Required.
const SOCKET_ENV: &str = "BOSS_EVENTS_SOCKET";
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
    // run without needing a working shell-pid lookup. On any failure
    // (env not set, payload not a JSON object) we forward the original
    // bytes unchanged so the shim stays best-effort and never blocks
    // the worker.
    let payload_line = match maybe_splice_run_id(&payload) {
        Ok(bytes) => bytes,
        Err(_) => payload,
    };

    let buffer_path = resolve_buffer_path();

    // Drain leftover buffered events FIRST (oldest first). Each drain
    // attempt uses a fresh single-shot connection with no retry: if
    // the engine is down, the very first drain connect fast-fails and
    // the rest stay queued for next time. Draining before the current
    // event preserves FIFO ordering on the engine's accept queue —
    // the current event's connect would otherwise sit in the backlog
    // ahead of the drained connections.
    if let Some(buf) = buffer_path.as_deref()
        && let Err(err) = drain_buffer(&socket_path, buf, deadline)
    {
        eprintln!("boss-event: drain of {} skipped: {err:#}", buf.display());
    }

    // Then send the current event, with bounded connect-retry and a
    // single mid-send reconnect on broken pipe. Both stages are bounded
    // by `deadline`, which was stamped before the drain above — so the
    // total across drain + this send can never exceed SHIM_TOTAL_BUDGET.
    match connect_with_retry(&socket_path, deadline) {
        Ok(stream) => match send_to_stream(stream, &payload_line) {
            Ok(()) => Ok(()),
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
                    match connect_once(&socket_path, write_timeout_for(deadline))
                        .and_then(|s| send_to_stream(s, &payload_line))
                    {
                        Ok(()) => Ok(()),
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

/// Wall time remaining until `deadline`, saturating at zero.
fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Whether the shim's total wall-clock budget has already run out.
fn budget_exhausted(deadline: Instant) -> bool {
    remaining(deadline).is_zero()
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
/// The shim never reads from this socket (it only writes and
/// half-closes), so no read timeout is set here.
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
        thread::sleep(delay.min(time_left));
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
fn send_to_stream(mut stream: UnixStream, payload: &[u8]) -> Result<()> {
    stream
        .write_all(payload)
        .context("writing hook payload to events socket")?;
    stream
        .shutdown(std::net::Shutdown::Write)
        .context("shutting down write half of events socket")?;
    Ok(())
}

/// Send one buffered event in its own connection. Used by drain. No
/// retry: a failure here means the engine just went down again and the
/// remaining buffered events should stay on disk for next time. Also
/// fails fast once `deadline` has passed, without attempting a connect.
fn send_one(socket_path: &str, payload: &[u8], deadline: Instant) -> Result<()> {
    if budget_exhausted(deadline) {
        return Err(anyhow!("shim wall-clock budget exhausted before send"));
    }
    let stream = connect_once(socket_path, write_timeout_for(deadline))?;
    send_to_stream(stream, payload)
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

/// Read the buffer line by line and try to deliver each event. Stops
/// at the first failure — including the shim's wall-clock budget
/// running out — and rewrites the file with the unsent suffix (FIFO).
/// Removes the file when fully drained. No-op when the buffer is
/// absent. If another shim is already draining, returns successfully
/// without waiting so that process can preserve FIFO order.
fn drain_buffer(socket_path: &str, buffer_path: &Path, deadline: Instant) -> Result<()> {
    if !buffer_path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(buffer_path)
        .with_context(|| format!("opening {} for drain", buffer_path.display()))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(err) => return Err(err).context("locking event buffer for drain"),
    }
    let _guard = LockGuard(&file);

    let lines = read_lines(&file)?;
    let mut sent = 0usize;
    let mut drain_err: Option<anyhow::Error> = None;
    for line in &lines {
        if budget_exhausted(deadline) {
            drain_err = Some(anyhow!("shim wall-clock budget exhausted during buffer drain"));
            break;
        }
        match send_one(socket_path, line, deadline) {
            Ok(()) => sent += 1,
            Err(err) => {
                drain_err = Some(err);
                break;
            }
        }
    }

    if sent == lines.len() {
        // All drained. Truncate the file to zero so a stale empty
        // buffer file doesn't keep showing up in `.boss/`.
        rewrite_lines(&file, &[])?;
    } else if sent > 0 {
        rewrite_lines(&file, &lines[sent..])?;
    }

    if let Some(err) = drain_err {
        return Err(err);
    }
    Ok(())
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
    fn connect_with_retry_skips_attempts_after_budget_expires() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("never-bound.sock");
        let start = Instant::now();
        let result = connect_with_retry(socket.to_str().unwrap(), Instant::now() - Duration::from_secs(1));

        assert!(result.is_err(), "an expired budget must reject the connect");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "an expired budget must skip the retry schedule"
        );
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
        let result = drain_buffer(socket.to_str().unwrap(), &buf, deadline);
        assert!(result.is_err(), "drain must surface connect failure");

        let contents = std::fs::read(&buf).unwrap();
        assert_eq!(contents, original);
    }

    #[test]
    fn drain_with_expired_budget_preserves_buffer() {
        let dir = tempfile::TempDir::new().unwrap();
        let buf = dir.path().join(".boss/events-pending.jsonl");
        std::fs::create_dir_all(buf.parent().unwrap()).unwrap();
        let original = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        std::fs::write(&buf, original).unwrap();

        let socket = dir.path().join("never-bound.sock");
        let start = Instant::now();
        let result = drain_buffer(socket.to_str().unwrap(), &buf, Instant::now() - Duration::from_secs(1));

        assert!(result.is_err(), "an expired budget must stop the drain");
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "an expired budget must not attempt a socket connect"
        );
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
        let result = send_to_stream(stream, &payload);
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
