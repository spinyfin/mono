//! Append-only engine intent log for coordinator chat prompts.
//!
//! The agent host's wire journal records every JSON-RPC line that crossed the
//! adapter, but cannot distinguish an operator prompt from an engine prompt
//! (both are `session/prompt` lines) and does not know about prompts that are
//! still queued or were withdrawn before being sent.
//!
//! Each coordinator generation therefore keeps an append-only `intent.jsonl`
//! file (mode `0600`) recording every prompt when it is queued (with origin,
//! client message id, text, and stored image references), when it is sent
//! (with the JSON-RPC `request_id` that carried it), and when it is withdrawn.
//! A message is appended here before the app is told it was accepted, so a
//! restart recovers the queue and every prompt's origin.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use boss_acp_client::schema::RequestId;
use boss_agent_host::journal::now_ms;
use serde::{Deserialize, Serialize};

use crate::queue::{PromptImageRef, PromptOrigin};

/// Read buffer capacity for replaying intent logs and wire journals.
///
/// Sized from the coordinator ACP spike measurement (mono#3922): the largest
/// recorded wire line was 27,587 bytes (`available_commands_update`). 64 KiB
/// keeps single lines from spilling across multiple `BufReader` refills.
pub const REPLAY_READ_BUFFER_BYTES: usize = 64 * 1024;

/// Why a queued prompt was withdrawn from the queue without being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawReason {
    /// Returned to the app composer as a draft when Stop was pressed.
    StopReturnedToComposer,
    /// Timed out waiting in the queue (for example a reset handoff delivery).
    Timeout,
    /// Withdrawn on engine restart because its in-memory correlation id no
    /// longer exists (for example a pending reset or authorization id).
    StaleOnRestart,
    /// Explicitly withdrawn by the caller before dispatch.
    CallerWithdrawn,
    #[serde(other)]
    Other,
}

/// One append-only record in the coordinator generation's intent log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentRecord {
    /// 1-based monotonic sequence number within this generation's intent log.
    pub seq: u64,
    /// Wall-clock milliseconds when the record was appended (display/forensics
    /// only; never used for ordering).
    pub ts_ms: u64,
    /// Event payload.
    #[serde(flatten)]
    pub kind: IntentRecordKind,
}

/// Discriminated payload for an [`IntentRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IntentRecordKind {
    /// A prompt entered the engine's turn queue.
    Queued {
        intent_id: u64,
        origin: PromptOrigin,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<PromptImageRef>,
    },
    /// A previously queued prompt was dispatched as `session/prompt` carrying
    /// `request_id`.
    Sent { intent_id: u64, request_id: RequestId },
    /// A queued prompt was removed without being sent to the agent.
    Withdrawn { intent_id: u64, reason: WithdrawReason },
}

impl IntentRecord {
    /// Returns the prompt `intent_id` referenced by this record.
    pub fn intent_id(&self) -> u64 {
        match &self.kind {
            IntentRecordKind::Queued { intent_id, .. }
            | IntentRecordKind::Sent { intent_id, .. }
            | IntentRecordKind::Withdrawn { intent_id, .. } => *intent_id,
        }
    }
}

/// Single-writer append-only handle to a generation's intent log (`0600`).
///
/// Unlike the host's wire journal (which starts fresh per host process), the
/// engine reopens the same generation's intent log across restarts and keeps
/// appending to it.
#[derive(Debug)]
pub struct IntentLog {
    path: PathBuf,
    file: File,
    next_seq: u64,
    next_intent_id: u64,
    failed: bool,
}

impl IntentLog {
    /// Opens an existing intent log for appending or creates a new private
    /// (`0600`) file at `path`.
    ///
    /// If the existing file ends with a torn final line (cut off by a prior
    /// engine crash), the trailing incomplete bytes are truncated away before
    /// new records are appended so the next write lands cleanly on a line
    /// boundary. Any corrupt record before the end is returned as an error.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;

        let (records, valid_end_offset) = scan_existing_file(&mut file)?;
        let file_len = file.metadata()?.len();
        if valid_end_offset < file_len {
            file.set_len(valid_end_offset)?;
        }
        file.seek(SeekFrom::End(0))?;

        let next_seq = records.last().map_or(1, |rec| rec.seq + 1);
        let next_intent_id = records.iter().map(IntentRecord::intent_id).max().map_or(1, |id| id + 1);

        Ok(Self {
            path: path.to_path_buf(),
            file,
            next_seq,
            next_intent_id,
            failed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sequence number the next appended record will carry.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Intent id the next [`Self::append_queued`] call will allocate.
    pub fn next_intent_id(&self) -> u64 {
        self.next_intent_id
    }

    /// Appends a `Queued` record with the current wall-clock timestamp.
    pub fn append_queued(
        &mut self,
        origin: PromptOrigin,
        text: impl Into<String>,
        client_message_id: Option<String>,
        images: Vec<PromptImageRef>,
    ) -> io::Result<IntentRecord> {
        self.append_queued_at(now_ms(), origin, text, client_message_id, images)
    }

    /// Appends a `Queued` record with an explicit `ts_ms` timestamp.
    pub fn append_queued_at(
        &mut self,
        ts_ms: u64,
        origin: PromptOrigin,
        text: impl Into<String>,
        client_message_id: Option<String>,
        images: Vec<PromptImageRef>,
    ) -> io::Result<IntentRecord> {
        let intent_id = self.next_intent_id;
        let record = self.append_kind(
            ts_ms,
            IntentRecordKind::Queued {
                intent_id,
                origin,
                text: text.into(),
                client_message_id,
                images,
            },
        )?;
        self.next_intent_id = intent_id + 1;
        Ok(record)
    }

    /// Appends a `Sent` record linking `intent_id` to `request_id`.
    pub fn append_sent(&mut self, intent_id: u64, request_id: RequestId) -> io::Result<IntentRecord> {
        self.append_sent_at(now_ms(), intent_id, request_id)
    }

    /// Appends a `Sent` record with an explicit `ts_ms` timestamp.
    pub fn append_sent_at(&mut self, ts_ms: u64, intent_id: u64, request_id: RequestId) -> io::Result<IntentRecord> {
        self.append_kind(ts_ms, IntentRecordKind::Sent { intent_id, request_id })
    }

    /// Appends a `Withdrawn` record for `intent_id`.
    pub fn append_withdrawn(&mut self, intent_id: u64, reason: WithdrawReason) -> io::Result<IntentRecord> {
        self.append_withdrawn_at(now_ms(), intent_id, reason)
    }

    /// Appends a `Withdrawn` record with an explicit `ts_ms` timestamp.
    pub fn append_withdrawn_at(
        &mut self,
        ts_ms: u64,
        intent_id: u64,
        reason: WithdrawReason,
    ) -> io::Result<IntentRecord> {
        self.append_kind(ts_ms, IntentRecordKind::Withdrawn { intent_id, reason })
    }

    fn append_kind(&mut self, ts_ms: u64, kind: IntentRecordKind) -> io::Result<IntentRecord> {
        if self.failed {
            return Err(io::Error::other(
                "intent log is unusable after an earlier write failure",
            ));
        }
        let record = IntentRecord {
            seq: self.next_seq,
            ts_ms,
            kind,
        };
        let mut encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
        encoded.push(b'\n');
        if let Err(err) = self.file.write_all(&encoded) {
            self.failed = true;
            return Err(err);
        }
        self.next_seq += 1;
        Ok(record)
    }
}

/// Reads an intent log back from `reader`.
///
/// Mirrors [`boss_agent_host::journal::read_records`]: blank lines are skipped,
/// a final line cut off mid-write is ignored, and any corrupt line before the
/// end returns `io::ErrorKind::InvalidData`.
pub fn read_intent_records(reader: impl BufRead) -> io::Result<Vec<IntentRecord>> {
    let mut records = Vec::new();
    let mut lines = reader.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<IntentRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) if lines.peek().is_none() => break,
            Err(err) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("intent record after seq {}: {err}", records.len()),
                ));
            }
        }
    }
    Ok(records)
}

/// Reads all intent records from `path` using a buffer sized from the spike's
/// measured line sizes ([`REPLAY_READ_BUFFER_BYTES`]).
pub fn read_intent_file(path: &Path) -> io::Result<Vec<IntentRecord>> {
    let file = File::open(path)?;
    read_intent_records(BufReader::with_capacity(REPLAY_READ_BUFFER_BYTES, file))
}

/// Reads all intent records from `path`, returning an empty vector if the file
/// does not exist yet.
pub fn read_intent_file_if_exists(path: &Path) -> io::Result<Vec<IntentRecord>> {
    match File::open(path) {
        Ok(file) => read_intent_records(BufReader::with_capacity(REPLAY_READ_BUFFER_BYTES, file)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// Scans an open intent log file from byte 0, returning the parsed records and
/// the byte offset immediately after the last valid newline-terminated record
/// (so any torn final bytes can be truncated before appending).
fn scan_existing_file(file: &mut File) -> io::Result<(Vec<IntentRecord>, u64)> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(REPLAY_READ_BUFFER_BYTES, file.try_clone()?);
    let mut records = Vec::new();
    let mut offset: u64 = 0;
    let mut valid_end: u64 = 0;
    let mut buf = Vec::new();

    loop {
        buf.clear();
        let read = reader.read_until(b'\n', &mut buf)?;
        if read == 0 {
            break;
        }
        let line_start = offset;
        offset += read as u64;
        let has_newline = buf.ends_with(b"\n");

        let Ok(line_str) = std::str::from_utf8(&buf) else {
            if !has_newline || is_at_eof(&mut reader)? {
                // Torn final record (cut off mid-UTF-8 sequence).
                return Ok((records, valid_end));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("intent log contains invalid UTF-8 at byte offset {line_start}"),
            ));
        };

        let trimmed = line_str.trim();
        if trimmed.is_empty() {
            if has_newline {
                valid_end = offset;
            }
            continue;
        }

        match serde_json::from_str::<IntentRecord>(trimmed) {
            Ok(record) => {
                if !has_newline {
                    // Valid JSON but missing its trailing newline: keep the
                    // record and let the caller know `valid_end` stopped before
                    // a newline would be needed; wait — if we treat an
                    // unterminated final JSON line as torn, we'd drop a valid
                    // record. Instead keep it and append a newline if needed.
                    // Even simpler: records are always written with `\n` in one
                    // `write_all`; an unterminated line can only happen if the
                    // write was cut off right before `\n`, in which case we
                    // still parse it and truncate nothing — wait, if we don't
                    // write `\n` then the next append would concatenate onto it!
                    // Writing `\n` in that rare case keeps the record intact.
                    records.push(record);
                    file.seek(SeekFrom::Start(offset))?;
                    file.write_all(b"\n")?;
                    offset += 1;
                    valid_end = offset;
                    break;
                }
                records.push(record);
                valid_end = offset;
            }
            Err(err) => {
                if !has_newline || is_at_eof(&mut reader)? {
                    return Ok((records, valid_end));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("intent record after seq {}: {err}", records.len()),
                ));
            }
        }
    }

    Ok((records, valid_end))
}

fn is_at_eof(reader: &mut BufReader<File>) -> io::Result<bool> {
    Ok(reader.fill_buf()?.is_empty())
}
