//! Stdout-JSONL progress ingress: the reader behind
//! [`boss_engine_driver::ProgressIngress::StdoutJsonl`].
//!
//! Boss has exactly two progress transports (see
//! `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`):
//!
//! - **`HookCallback`** — Claude's settings-file hooks fan every lifecycle/tool
//!   event out to the `boss-event` shim, which forwards each payload to the
//!   engine's unix events socket. That path is `boss_engine::events_socket`
//!   and is untouched by this crate.
//! - **`StdoutJsonl`** — the worker process itself emits one JSON envelope per
//!   line on stdout. Nothing forwards it; the engine has to read the stream.
//!   That is this crate.
//!
//! Without this reader a hookless driver has no progress signal at all: the
//! socket ingress is only ever fed by a Claude hook, so a Codex worker would
//! emit no events, never advance the activity machine, and never complete.
//!
//! # Shape
//!
//! [`StdoutJsonlProgressReader`] is a pull-style cursor over any
//! [`AsyncRead`] — a child process's `ChildStdout`, an SSH channel, or a pipe
//! in a test. Each [`StdoutJsonlProgressReader::next_event`] call yields the
//! next envelope that a **driver** (not this crate) recognised, or `None` at
//! end of stream. The driver is injected as a `dyn AgentDriver`, so which
//! envelopes are recognised — and how they map onto
//! [`WorkerEvent`] — stays entirely in
//! [`AgentDriver::normalize_progress_event`], whose signature this crate does
//! not touch.
//!
//! Pull-style rather than a channel or a callback on purpose: the consumer
//! paces the reader, so events reach the activity machine in stream order with
//! no intermediate queue to bound, drop from, or reorder.
//!
//! # Robustness
//!
//! The stream this reads is unversioned and comes from a process Boss does not
//! control. Codex has added fields and event variants across minor releases
//! without announcement, workers interleave non-JSON chatter onto stdout, and a
//! killed worker closes the pipe mid-envelope. A reader that panicked or bailed
//! on any of those would be strictly worse than one that reports nothing, so
//! every anomaly below is counted in [`ReaderStats`] and skipped, never fatal:
//!
//! - **Unknown/unparseable envelopes** — a line the driver rejects (unknown
//!   variant, missing field, added shape) is skipped; the stream continues.
//! - **Interleaved non-JSON output** — log lines, progress bars, and shell
//!   noise are skipped the same way.
//! - **Partial lines** — envelopes split across read boundaries are reassembled;
//!   only a newline (or end of stream) ends a line.
//! - **Close mid-envelope** — an unterminated trailing fragment is parsed if it
//!   happens to be complete JSON and dropped if it isn't.
//! - **Unbounded lines** — a line longer than [`ReaderConfig::max_line_bytes`]
//!   is discarded up to its terminator instead of growing the buffer without
//!   limit, so a worker that emits a giant blob (or a stream carrying no
//!   newlines at all) cannot exhaust engine memory.
//! - **Invalid UTF-8** — decoded lossily, which turns into a JSON parse failure
//!   and is skipped like any other non-JSON line.

use std::io::ErrorKind;
use std::sync::Arc;

use boss_engine_driver::AgentDriver;
use boss_protocol::WorkerEvent;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

#[cfg(test)]
mod tests;

/// Default per-line ceiling: 1 MiB.
///
/// Two orders of magnitude above any real envelope (Codex's largest observed
/// item — a `command_execution` carrying `aggregated_output` — is kilobytes),
/// while still bounding what a runaway writer can make the engine buffer.
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;

/// Tunables for [`StdoutJsonlProgressReader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderConfig {
    /// Longest line the reader will buffer before discarding it. A line that
    /// exceeds this is dropped up to and including its newline; the reader
    /// resynchronises on the next line rather than truncating mid-envelope and
    /// handing the driver a corrupt prefix.
    pub max_line_bytes: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

/// One progress event decoded off the stdout stream.
///
/// Deliberately the same two pieces of information the hook/socket ingress
/// extracts per payload (`boss_engine::events_socket::IncomingHookEvent`'s
/// `event` + `transcript_path`), so the engine-side adapter is a struct
/// re-shuffle and both transports reach the activity machine carrying the same
/// shapes. `run_id` is not on here: the stdout stream has no run correlation
/// field to read — the caller owns the process it is reading and therefore
/// already knows which run it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressEnvelope {
    /// The driver-normalised event that drives the activity machine.
    pub event: WorkerEvent,
    /// Where this driver's transcript for the session lives, if the envelope
    /// carried it. Resolved through
    /// [`AgentDriver::transcript_path_for_session`], exactly as the socket
    /// ingress does, so live-status transcript discovery works identically on
    /// both transports. `None` on envelopes that don't carry it; the engine
    /// retries on a later one.
    pub transcript_path: Option<String>,
}

/// What the reader saw on the stream, for logging and assertions.
///
/// Every counter except `events_emitted` records something the reader
/// *tolerated*. They exist so a hookless worker that goes quiet can be
/// diagnosed — "3000 lines read, 0 events emitted, 3000 unrecognised" is a
/// driver/normaliser mismatch, whereas "0 lines read" is a stream that never
/// produced anything.
///
/// The reader builds this by starting from [`Default`] and incrementing as it
/// goes; the derived builder exists to satisfy the repo's giant-struct
/// convention, so adding a counter here stays additive for anyone who does
/// construct one explicitly.
#[derive(bon::Builder, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderStats {
    /// Lines framed off the stream, including ones that were skipped.
    /// Excludes lines dropped for exceeding `max_line_bytes` (counted by
    /// `oversized_lines` instead — nothing was ever framed).
    pub lines_read: u64,
    /// Envelopes the driver recognised and the reader yielded.
    pub events_emitted: u64,
    /// Empty or whitespace-only lines.
    pub blank_lines: u64,
    /// Lines that were not valid JSON — interleaved worker chatter, a
    /// truncated final fragment, or invalid UTF-8.
    pub non_json_lines: u64,
    /// Valid JSON the driver declined to normalise: an unknown variant, a
    /// missing field, or a shape added by a newer CLI release.
    pub unrecognised_envelopes: u64,
    /// Lines discarded for exceeding [`ReaderConfig::max_line_bytes`].
    pub oversized_lines: u64,
    /// Trailing fragments seen at end of stream with no terminating newline —
    /// i.e. the stream closed mid-envelope. At most 1 per stream.
    pub unterminated_tails: u64,
    /// Set when the read failed with an I/O error and the reader stopped early
    /// rather than at a clean end of stream.
    pub ended_with_io_error: bool,
}

/// One framed line, or the reason there wasn't one.
enum RawLine {
    /// A newline-terminated line, terminator stripped.
    Complete(Vec<u8>),
    /// End of stream with unterminated bytes buffered — the writer closed
    /// mid-envelope (or simply omitted a trailing newline).
    Trailing(Vec<u8>),
    /// A line over the byte ceiling. Discarded rather than truncated: half an
    /// envelope is not a smaller envelope, it is a corrupt one.
    Oversized,
    /// Clean end of stream with nothing buffered.
    Eof,
}

/// Reads newline-delimited JSON progress envelopes off a worker's stdout and
/// normalises each through the driver that produced it.
///
/// See the crate docs for the transport split and the tolerated-anomaly list.
pub struct StdoutJsonlProgressReader<R> {
    reader: BufReader<R>,
    driver: Arc<dyn AgentDriver>,
    config: ReaderConfig,
    stats: ReaderStats,
}

impl<R: AsyncRead + Unpin> StdoutJsonlProgressReader<R> {
    /// Wrap `stream` with the default [`ReaderConfig`], decoding through
    /// `driver`.
    ///
    /// `driver` is whatever the caller resolved out of the registry for this
    /// run — this crate never names a concrete driver, so the same reader
    /// serves every `StdoutJsonl` backend.
    pub fn new(stream: R, driver: Arc<dyn AgentDriver>) -> Self {
        Self::with_config(stream, driver, ReaderConfig::default())
    }

    /// [`Self::new`] with an explicit config.
    pub fn with_config(stream: R, driver: Arc<dyn AgentDriver>, config: ReaderConfig) -> Self {
        Self {
            reader: BufReader::new(stream),
            driver,
            config,
            stats: ReaderStats::default(),
        }
    }

    /// What the reader has seen so far.
    pub fn stats(&self) -> ReaderStats {
        self.stats
    }

    /// Yield the next envelope the driver recognised.
    ///
    /// Skips — and counts — blank lines, non-JSON output, and envelopes the
    /// driver declines, so a single unparseable line never ends the stream.
    /// Returns `None` at end of stream, and also on an I/O error (recorded in
    /// [`ReaderStats::ended_with_io_error`]): once the pipe is broken there is
    /// nothing left to read, and returning `None` lets the caller finish its
    /// loop normally instead of unwinding.
    pub async fn next_event(&mut self) -> Option<ProgressEnvelope> {
        loop {
            let bytes = match self.next_raw_line().await {
                RawLine::Complete(bytes) => {
                    self.stats.lines_read += 1;
                    bytes
                }
                RawLine::Trailing(bytes) => {
                    self.stats.lines_read += 1;
                    self.stats.unterminated_tails += 1;
                    tracing::debug!(
                        bytes = bytes.len(),
                        "stdout progress: stream ended without a trailing newline; \
                         parsing the residual fragment",
                    );
                    bytes
                }
                RawLine::Oversized => {
                    self.stats.oversized_lines += 1;
                    tracing::warn!(
                        max_line_bytes = self.config.max_line_bytes,
                        "stdout progress: discarded an over-long line; resynchronising at the next newline",
                    );
                    continue;
                }
                RawLine::Eof => return None,
            };

            // Lossy on purpose: invalid UTF-8 becomes replacement characters,
            // which fail the JSON parse below and are skipped like any other
            // non-JSON line. Erroring here would let one bad byte end a
            // worker's entire progress stream.
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            if text.is_empty() {
                self.stats.blank_lines += 1;
                continue;
            }

            let raw: serde_json::Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(err) => {
                    self.stats.non_json_lines += 1;
                    tracing::debug!(
                        %err,
                        "stdout progress: skipping a non-JSON line (interleaved worker output or a truncated fragment)",
                    );
                    continue;
                }
            };

            match self.driver.normalize_progress_event(&raw) {
                Ok(event) => {
                    self.stats.events_emitted += 1;
                    let transcript_path = self.driver.transcript_path_for_session(&raw);
                    return Some(ProgressEnvelope { event, transcript_path });
                }
                Err(err) => {
                    // The expected steady state for a stream that mixes
                    // progress envelopes with variants the driver has no
                    // mapping for. Debug, not warn: an unrecognised variant is
                    // this transport's normal case, not a fault.
                    self.stats.unrecognised_envelopes += 1;
                    tracing::debug!(
                        %err,
                        "stdout progress: driver declined an envelope; skipping",
                    );
                    continue;
                }
            }
        }
    }

    /// Frame one line off the stream, enforcing the byte ceiling.
    ///
    /// Reassembles across read boundaries (a `fill_buf` may hand back any
    /// fraction of a line), so a partial envelope is never handed on as if it
    /// were whole.
    async fn next_raw_line(&mut self) -> RawLine {
        let mut line: Vec<u8> = Vec::new();
        // Once a line has blown the ceiling we stop accumulating and just burn
        // bytes until the newline, so the discard costs no memory either.
        let mut overflowed = false;

        loop {
            // Copy the interesting slice out and end the borrow before
            // `consume`, which needs the reader mutably again.
            let (chunk, terminated) = {
                let available = match self.reader.fill_buf().await {
                    Ok(available) => available,
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(err) => {
                        self.stats.ended_with_io_error = true;
                        tracing::warn!(
                            %err,
                            "stdout progress: read failed; ending the progress stream",
                        );
                        return RawLine::Eof;
                    }
                };
                if available.is_empty() {
                    return if overflowed {
                        // The over-long line ran to end of stream. Nothing was
                        // buffered and there is no next line to resync to.
                        RawLine::Oversized
                    } else if line.is_empty() {
                        RawLine::Eof
                    } else {
                        RawLine::Trailing(line)
                    };
                }
                match available.iter().position(|byte| *byte == b'\n') {
                    Some(index) => (available[..=index].to_vec(), true),
                    None => (available.to_vec(), false),
                }
            };
            self.reader.consume(chunk.len());

            if terminated {
                // Drop the terminator; `\r` (if any) is handled by the trim in
                // `next_event`.
                let payload = &chunk[..chunk.len() - 1];
                if overflowed || line.len() + payload.len() > self.config.max_line_bytes {
                    return RawLine::Oversized;
                }
                line.extend_from_slice(payload);
                return RawLine::Complete(line);
            }

            // Check before extending, so the ceiling bounds what is ever held
            // rather than what is held after one more chunk.
            if !overflowed {
                if line.len() + chunk.len() > self.config.max_line_bytes {
                    overflowed = true;
                    line = Vec::new();
                } else {
                    line.extend_from_slice(&chunk);
                }
            }
        }
    }
}
