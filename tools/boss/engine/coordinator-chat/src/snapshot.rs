//! Versioned reducer snapshot cache and restart recovery.
//!
//! Replaying a long-lived coordinator generation from sequence 1 grows
//! linearly with the generation's wire journal. To keep engine restarts fast,
//! the engine periodically writes a versioned [`ReducerSnapshot`] capturing
//! the reducer's full state at `(wire_seq, intent_seq)`.
//!
//! A snapshot is strictly a cache:
//! - If the file is missing, unreadable, truncated, structurally invalid, from
//!   a different [`SNAPSHOT_SCHEMA_VERSION`], or ahead of the wire/intent log
//!   tips, recovery discards the cache and replays cleanly from the start.
//! - Writes are atomic (write to a private `0600` sibling temporary file,
//!   flush, then rename over the destination) so a crash mid-write never
//!   corrupts the previously saved snapshot.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use boss_agent_host::journal::WireRecord;
use boss_engine_utils::atomic_blob::write_blob_atomic_with_mode;
use boss_protocol::{CoordinatorChatConnectionState, CoordinatorChatItem};
use serde::{Deserialize, Serialize};

use crate::intent::{IntentRecord, REPLAY_READ_BUFFER_BYTES, read_intent_file_if_exists};
use crate::queue::{QueuedPrompt, TurnQueue};
use crate::redact::CredentialRedactor;
use crate::reducer::{ActiveStreamSnapshot, ConversationReducer, WireTracker};
use boss_agent_host::journal::scan_jsonl;

/// Current on-disk schema version of [`ReducerSnapshot`].
///
/// Bump this whenever the serialized shape or semantics of [`ReducerSnapshot`]
/// changes so older snapshots cleanly fall back to full replay.
pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
// Version 2 adds `TurnQueue` unbound-prompt tracking and the withdrawn flag on
// dispatched prompts. A version 1 snapshot taken while an unbound
// `session/prompt` was in flight would load as idle, so it is a cache miss.

/// Recommended wire-record interval between periodic reducer snapshots.
///
/// Sized to bound restart tail-replay latency from the synthetic 250-turn
/// measurement (`reducer_tests::restart_and_snapshot_tests::measure_synthetic_spike_sized_journal_replay`):
/// - Full replay: 6,259 wire records (2,216,798 bytes) in ~133 ms (~21 us/wire
///   record, ~0.53 ms/turn), so a 500-record (~20-turn) tail replays in ~10 ms
///   on top of snapshot load.
/// - Snapshot write cost: `ReducerSnapshot` serializes the full item list
///   (~1.7 KB/turn; 404,902 bytes and ~6.7 ms save at 240 turns), so late in a
///   250-turn generation a 500-record window covers ~177 KB of wire traffic
///   while writing a ~405 KB snapshot (~2.3x per-window write amplification;
///   cumulative snapshot write volume grows quadratically with generation
///   length, ~2.5 MB across 12 snapshots over 250 turns). Callers on very
///   long-lived generations can widen the interval or gate snapshots on wire
///   bytes written since the previous snapshot.
pub const DEFAULT_SNAPSHOT_INTERVAL_RECORDS: u64 = 500;

/// Sequence boundary and sizing parameters captured in a [`ReducerSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCursor {
    pub wire_seq: u64,
    pub intent_seq: u64,
    pub tool_text_limit: usize,
}

/// Projected conversation items, connection state, and active stream buffers
/// captured in a [`ReducerSnapshot`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotProjection {
    pub state: CoordinatorChatConnectionState,
    pub items: Vec<CoordinatorChatItem>,
    #[serde(default)]
    pub compaction_indices: HashMap<String, usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) streaming_message: Option<ActiveStreamSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) streaming_thought: Option<ActiveStreamSnapshot>,
}

/// Versioned serialized cache of a [`ConversationReducer`] at a specific
/// `(wire_seq, intent_seq)` boundary.
///
/// The generation's secret credential list is never stored in the snapshot;
/// callers supply the [`CredentialRedactor`] when restoring. Because an
/// in-progress streaming holdback buffer or queued engine prompt may still hold
/// unredacted text, snapshot files are created with mode `0600`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReducerSnapshot {
    pub schema_version: u32,
    #[serde(flatten)]
    pub cursor: SnapshotCursor,
    #[serde(flatten)]
    pub projection: SnapshotProjection,
    pub(crate) wire: WireTracker,
    pub queue: TurnQueue,
}

#[derive(Debug, Deserialize)]
struct SnapshotVersionHeader {
    schema_version: u32,
}

/// Result of attempting to load a snapshot cache file or byte slice.
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotLoad {
    /// Snapshot parsed and matched [`SNAPSHOT_SCHEMA_VERSION`].
    Loaded(Box<ReducerSnapshot>),
    /// Snapshot file does not exist.
    Missing,
    /// Snapshot file was unreadable, truncated, or malformed JSON.
    Corrupt { reason: String },
    /// Snapshot carried a different `schema_version` than this build supports.
    VersionMismatch { found: u32, expected: u32 },
}

/// How recovery used (or bypassed) the snapshot cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotLoadStatus {
    /// Loaded the snapshot and replayed only records after `(wire_seq, intent_seq)`.
    Hit { wire_seq: u64, intent_seq: u64 },
    /// Snapshot file did not exist; replayed from the start.
    Missing,
    /// Snapshot was unreadable, corrupt, or failed structural checks; replayed from the start.
    Corrupt { reason: String },
    /// Snapshot schema version did not match; replayed from the start.
    VersionMismatch { found: u32, expected: u32 },
    /// Snapshot was written with a different tool text limit than the one
    /// requested, so its projected tool items would differ; replayed from the start.
    ToolTextLimitMismatch { found: usize, expected: usize },
    /// Snapshot claimed a sequence number beyond the journal/intent log tips; replayed from the start.
    StaleAheadOfJournal {
        snapshot_wire_seq: u64,
        journal_wire_seq: u64,
        snapshot_intent_seq: u64,
        journal_intent_seq: u64,
    },
    /// No snapshot path/bytes were provided; replayed from the start.
    NotConfigured,
}

/// Controls whether recovery immediately re-queues `IntentRecord::Sent` entries
/// that have not appeared in the replayed wire records.
///
/// Re-queueing an unsent dispatch is only safe once replay has reached the
/// agent host's live boundary (or after the host has exited). If a live host
/// may still be appending to the wire journal, pass
/// [`UnsentDispatchPolicy::DeferUntilLiveBoundary`] so `Sent` intents stay in
/// `dispatched_awaiting_wire` until the caller catches up to the live stream
/// and invokes [`ConversationReducer::reconcile_unsent_dispatches`] explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsentDispatchPolicy {
    /// Keep unconfirmed `Sent` intents in `dispatched_awaiting_wire` until the
    /// caller reaches the agent host's live boundary and calls
    /// [`ConversationReducer::reconcile_unsent_dispatches`].
    DeferUntilLiveBoundary,
    /// Immediately re-queue any `Sent` intent absent from the replayed wire
    /// records. Use only when the replayed wire records already cover up to
    /// the host's live boundary (or after the host has exited).
    ReconcileNow,
}

/// Full output of restarting and recovering a coordinator conversation from
/// optional snapshot + wire journal + intent log.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    pub reducer: ConversationReducer,
    pub snapshot_status: SnapshotLoadStatus,
    pub wire_records_replayed: usize,
    pub intent_records_replayed: usize,
    /// Prompts that were logged as `Sent` in the intent log before a crash but
    /// never appeared on the wire journal, and were re-queued (populated only
    /// when [`UnsentDispatchPolicy::ReconcileNow`] was requested).
    pub recovered_unsent_intents: Vec<QueuedPrompt>,
}

/// Atomically saves `snapshot` to `path` with mode `0600`.
pub fn save_snapshot(path: &Path, snapshot: &ReducerSnapshot) -> io::Result<()> {
    let encoded = serde_json::to_vec(snapshot).map_err(io::Error::other)?;
    write_blob_atomic_with_mode(path, &encoded, 0o600)
}

/// Loads a [`ReducerSnapshot`] from `path`, treating every error as a cache
/// miss (`Missing`, `VersionMismatch`, or `Corrupt`).
pub fn load_snapshot(path: &Path) -> SnapshotLoad {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return SnapshotLoad::Missing,
        Err(err) => {
            return SnapshotLoad::Corrupt {
                reason: format!("reading snapshot: {err}"),
            };
        }
    };
    load_snapshot_bytes(&bytes)
}

/// Parses a [`ReducerSnapshot`] from raw JSON bytes.
///
/// Checks `schema_version` first so a snapshot written by another version with
/// a different field layout reports [`SnapshotLoad::VersionMismatch`] instead
/// of [`SnapshotLoad::Corrupt`].
pub fn load_snapshot_bytes(bytes: &[u8]) -> SnapshotLoad {
    let header: SnapshotVersionHeader = match serde_json::from_slice(bytes) {
        Ok(header) => header,
        Err(err) => {
            return SnapshotLoad::Corrupt {
                reason: format!("decoding snapshot header: {err}"),
            };
        }
    };
    if header.schema_version != SNAPSHOT_SCHEMA_VERSION {
        return SnapshotLoad::VersionMismatch {
            found: header.schema_version,
            expected: SNAPSHOT_SCHEMA_VERSION,
        };
    }
    match serde_json::from_slice::<ReducerSnapshot>(bytes) {
        Ok(snapshot) => SnapshotLoad::Loaded(Box::new(snapshot)),
        Err(err) => SnapshotLoad::Corrupt {
            reason: format!("decoding snapshot body: {err}"),
        },
    }
}

/// Reads a wire journal file using [`REPLAY_READ_BUFFER_BYTES`], returning an
/// empty vector if the file does not exist yet. A final record cut off
/// mid-write (including mid-UTF-8 codepoint) is dropped.
pub fn read_wire_file_if_exists(path: &Path) -> io::Result<Vec<WireRecord>> {
    match File::open(path) {
        Ok(file) => Ok(scan_jsonl::<WireRecord>(
            BufReader::with_capacity(REPLAY_READ_BUFFER_BYTES, file),
            "wire journal",
        )?
        .records),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

/// Recovers a [`ConversationReducer`] from on-disk generation files:
/// optional snapshot cache, wire journal, and optional intent log.
///
/// Uses [`REPLAY_READ_BUFFER_BYTES`] buffers for reading the wire journal and
/// intent log. Any snapshot problem (missing, unreadable, corrupt, wrong
/// version, invalid internal indices, or sequence ahead of the logs) falls
/// back transparently to full replay from sequence 1.
///
/// Pass [`UnsentDispatchPolicy::DeferUntilLiveBoundary`] if the agent host may
/// still be appending to `wire_path`, and call
/// [`ConversationReducer::reconcile_unsent_dispatches`] only after catching up
/// to the host's live boundary.
pub fn recover_from_files(
    wire_path: &Path,
    intent_path: Option<&Path>,
    snapshot_path: Option<&Path>,
    redactor: CredentialRedactor,
    tool_text_limit: usize,
    unsent_policy: UnsentDispatchPolicy,
) -> io::Result<RecoveryResult> {
    let wire_records = read_wire_file_if_exists(wire_path)?;
    let intent_records = match intent_path {
        Some(path) => read_intent_file_if_exists(path)?,
        None => Vec::new(),
    };
    let loaded = snapshot_path.map(load_snapshot);
    Ok(recover_from_loaded(
        &wire_records,
        &intent_records,
        loaded,
        redactor,
        tool_text_limit,
        unsent_policy,
    ))
}

/// Applies already-filtered tail records (`wire_tail` with `seq > snapshot.wire_seq`
/// and `intent_tail` with `seq > snapshot.intent_seq`) onto a loaded snapshot.
///
/// This mirrors the live `boss-agent-host` reconnect path where the host seeks
/// directly to `replay_from = snapshot.wire_seq + 1` and streams only the tail.
pub fn recover_from_snapshot_and_tail(
    snapshot_bytes: &[u8],
    wire_tail: &[WireRecord],
    intent_tail: &[IntentRecord],
    redactor: CredentialRedactor,
    tool_text_limit: usize,
    unsent_policy: UnsentDispatchPolicy,
) -> Result<RecoveryResult, SnapshotLoadStatus> {
    let snapshot = match load_snapshot_bytes(snapshot_bytes) {
        SnapshotLoad::Loaded(snapshot) => snapshot,
        SnapshotLoad::Missing => return Err(SnapshotLoadStatus::Missing),
        SnapshotLoad::Corrupt { reason } => return Err(SnapshotLoadStatus::Corrupt { reason }),
        SnapshotLoad::VersionMismatch { found, expected } => {
            return Err(SnapshotLoadStatus::VersionMismatch { found, expected });
        }
    };
    if snapshot.cursor.tool_text_limit != tool_text_limit {
        return Err(SnapshotLoadStatus::ToolTextLimitMismatch {
            found: snapshot.cursor.tool_text_limit,
            expected: tool_text_limit,
        });
    }
    let snap_wire_seq = snapshot.cursor.wire_seq;
    let snap_intent_seq = snapshot.cursor.intent_seq;
    let mut reducer = ConversationReducer::from_snapshot(*snapshot, redactor)
        .map_err(|reason| SnapshotLoadStatus::Corrupt { reason })?;

    let mut intent_records_replayed = 0usize;
    for intent in intent_tail {
        if intent.seq > snap_intent_seq {
            reducer.apply_intent_record(intent);
            intent_records_replayed += 1;
        }
    }

    let mut wire_records_replayed = 0usize;
    for record in wire_tail {
        if record.seq > snap_wire_seq {
            reducer.apply_record(record);
            wire_records_replayed += 1;
        }
    }

    let recovered_unsent_intents = match unsent_policy {
        UnsentDispatchPolicy::ReconcileNow => reducer.reconcile_unsent_dispatches(),
        UnsentDispatchPolicy::DeferUntilLiveBoundary => Vec::new(),
    };
    Ok(RecoveryResult {
        reducer,
        snapshot_status: SnapshotLoadStatus::Hit {
            wire_seq: snap_wire_seq,
            intent_seq: snap_intent_seq,
        },
        wire_records_replayed,
        intent_records_replayed,
        recovered_unsent_intents,
    })
}

/// In-memory equivalent of [`recover_from_files`], accepting pre-loaded slices
/// and optional raw snapshot JSON bytes.
pub fn recover_from_slices(
    wire_records: &[WireRecord],
    intent_records: &[IntentRecord],
    snapshot_bytes: Option<&[u8]>,
    redactor: CredentialRedactor,
    tool_text_limit: usize,
    unsent_policy: UnsentDispatchPolicy,
) -> RecoveryResult {
    let loaded = snapshot_bytes.map(load_snapshot_bytes);
    recover_from_loaded(
        wire_records,
        intent_records,
        loaded,
        redactor,
        tool_text_limit,
        unsent_policy,
    )
}

fn recover_from_loaded(
    wire_records: &[WireRecord],
    intent_records: &[IntentRecord],
    loaded: Option<SnapshotLoad>,
    redactor: CredentialRedactor,
    tool_text_limit: usize,
    unsent_policy: UnsentDispatchPolicy,
) -> RecoveryResult {
    let journal_wire_seq = wire_records.iter().map(|rec| rec.seq).max().unwrap_or(0);
    let journal_intent_seq = intent_records.iter().map(|rec| rec.seq).max().unwrap_or(0);

    let (mut reducer, snapshot_status, start_wire_seq, start_intent_seq) = match loaded {
        None => (
            ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
            SnapshotLoadStatus::NotConfigured,
            0,
            0,
        ),
        Some(SnapshotLoad::Missing) => (
            ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
            SnapshotLoadStatus::Missing,
            0,
            0,
        ),
        Some(SnapshotLoad::Corrupt { reason }) => (
            ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
            SnapshotLoadStatus::Corrupt { reason },
            0,
            0,
        ),
        Some(SnapshotLoad::VersionMismatch { found, expected }) => (
            ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
            SnapshotLoadStatus::VersionMismatch { found, expected },
            0,
            0,
        ),
        Some(SnapshotLoad::Loaded(snapshot)) => {
            if snapshot.cursor.tool_text_limit != tool_text_limit {
                let status = SnapshotLoadStatus::ToolTextLimitMismatch {
                    found: snapshot.cursor.tool_text_limit,
                    expected: tool_text_limit,
                };
                (
                    ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
                    status,
                    0,
                    0,
                )
            } else if snapshot.cursor.wire_seq > journal_wire_seq || snapshot.cursor.intent_seq > journal_intent_seq {
                let status = SnapshotLoadStatus::StaleAheadOfJournal {
                    snapshot_wire_seq: snapshot.cursor.wire_seq,
                    journal_wire_seq,
                    snapshot_intent_seq: snapshot.cursor.intent_seq,
                    journal_intent_seq,
                };
                (
                    ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
                    status,
                    0,
                    0,
                )
            } else {
                let snap_wire_seq = snapshot.cursor.wire_seq;
                let snap_intent_seq = snapshot.cursor.intent_seq;
                match ConversationReducer::from_snapshot(*snapshot, redactor.clone()) {
                    Ok(restored) => (
                        restored,
                        SnapshotLoadStatus::Hit {
                            wire_seq: snap_wire_seq,
                            intent_seq: snap_intent_seq,
                        },
                        snap_wire_seq,
                        snap_intent_seq,
                    ),
                    Err(reason) => (
                        ConversationReducer::with_tool_text_limit(redactor, tool_text_limit),
                        SnapshotLoadStatus::Corrupt { reason },
                        0,
                        0,
                    ),
                }
            }
        }
    };

    let mut intent_records_replayed = 0usize;
    for intent in intent_records {
        if intent.seq > start_intent_seq {
            reducer.apply_intent_record(intent);
            intent_records_replayed += 1;
        }
    }

    let mut wire_records_replayed = 0usize;
    for record in wire_records {
        if record.seq > start_wire_seq {
            reducer.apply_record(record);
            wire_records_replayed += 1;
        }
    }

    let recovered_unsent_intents = match unsent_policy {
        UnsentDispatchPolicy::ReconcileNow => reducer.reconcile_unsent_dispatches(),
        UnsentDispatchPolicy::DeferUntilLiveBoundary => Vec::new(),
    };

    RecoveryResult {
        reducer,
        snapshot_status,
        wire_records_replayed,
        intent_records_replayed,
        recovered_unsent_intents,
    }
}
